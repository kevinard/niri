use std::cell::RefCell;

use pango::{EllipsizeMode, FontDescription};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::utils::{Logical, Point, Transform};

use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

const HEIGHT: f64 = 28.;
const PADDING: f64 = 10.;

#[derive(Debug, Default)]
pub(super) struct OverviewCaption {
    texture: RefCell<Option<(Key, Option<TextureBuffer<GlesTexture>>)>>,
}

#[derive(Debug, PartialEq)]
struct Key {
    renderer: ContextId<GlesTexture>,
    text: String,
    active: bool,
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
        active: bool,
        width: f64,
        scale: f64,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        // Round the width down in steps so a zoom redraw does not repeatedly
        // rasterize the same short caption.
        let key = Key {
            renderer: renderer.context_id(),
            text: caption(index, name),
            active,
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
}

fn caption(index: usize, name: Option<&str>) -> String {
    let name: String = name
        .unwrap_or_default()
        .chars()
        .take(128)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let name = name.trim();
    if name.is_empty() {
        format!("{index}")
    } else {
        format!("{index}   {name}")
    }
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
    let width = (f64::from(label.pixel_size().0) + 2. * PADDING)
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
    if key.active {
        cr.set_source_rgb(0.72, 0.88, 0.80);
    } else {
        cr.set_source_rgba(0.08, 0.14, 0.15, 0.92);
    }
    cr.fill()?;

    let label = layout(&cr, key);
    label.set_width(((width - 2. * PADDING) * f64::from(pango::SCALE)) as i32);
    let (w, h) = label.pixel_size();
    cr.move_to(
        ((width - f64::from(w)) / 2.).max(PADDING),
        (HEIGHT - f64::from(h)) / 2.,
    );
    if key.active {
        cr.set_source_rgb(0.09, 0.19, 0.16);
    } else {
        cr.set_source_rgb(0.91, 0.96, 0.93);
    }
    pangocairo::functions::show_layout(&cr, &label);
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
        assert_eq!(caption(2, None), "2");
        assert_eq!(caption(3, Some("  ")), "3");
        assert_eq!(caption(4, Some("Field\nnotes")), "4   Field notes");
        assert_eq!(caption(1, Some(&"x".repeat(160))).len(), 132);
    }
}
