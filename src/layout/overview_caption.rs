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

#[derive(Debug)]
pub(super) struct OverviewCaption {
    title: PreviewLabel,
    outside_view: RefCell<[SolidColorBuffer; 2]>,
    frame: RefCell<[BorderRenderElement; 2]>,
    fallback_fill: RefCell<SolidColorBuffer>,
}

impl Default for OverviewCaption {
    fn default() -> Self {
        Self {
            title: PreviewLabel::default(),
            outside_view: RefCell::default(),
            frame: RefCell::new(std::array::from_fn(|_| BorderRenderElement::empty())),
            fallback_fill: RefCell::default(),
        }
    }
}

#[derive(Debug, PartialEq)]
struct Key {
    renderer: ContextId<GlesTexture>,
    text: String,
    status: Option<&'static str>,
    background: Color,
    ink: Color,
    thumbnail: bool,
    width: u32,
    scale: f64,
}

#[derive(Debug, Default)]
pub(super) struct PreviewLabel {
    texture: RefCell<Option<(Key, Option<TextureBuffer<GlesTexture>>)>>,
}

impl PreviewLabel {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &self,
        renderer: &mut GlesRenderer,
        text: String,
        status: Option<&'static str>,
        background: Color,
        thumbnail: bool,
        width: f64,
        scale: f64,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        let ink = if is_light(background) {
            Color::new_unpremul(0.06, 0.13, 0.11, 1.)
        } else {
            Color::new_unpremul(0.95, 0.98, 0.95, 1.)
        };
        let key = Key {
            renderer: renderer.context_id(),
            text,
            status,
            background,
            ink,
            thumbnail,
            width: if thumbnail {
                width as u32
            } else {
                (width as u32 / 8) * 8
            },
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

pub(super) fn caption_background(accent: Color, backdrop: Color, active: bool) -> Color {
    if active {
        Color::new_unpremul(accent.r, accent.g, accent.b, 1.)
    } else if is_light(backdrop) {
        Color::new_unpremul(0.92, 0.95, 0.93, 1.)
    } else {
        Color::new_unpremul(0.08, 0.14, 0.15, 1.)
    }
}

impl OverviewCaption {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &self,
        renderer: &mut GlesRenderer,
        index: usize,
        name: Option<&str>,
        empty: bool,
        background: Color,
        width: f64,
        scale: f64,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        self.title.render(
            renderer,
            caption(index, name, empty),
            None,
            background,
            false,
            width,
            scale,
            location,
            alpha,
        )
    }

    pub(super) fn render_outside_view(
        &self,
        strip: std::ops::Range<f64>,
        visible: &std::ops::Range<f64>,
        vertical: std::ops::Range<f64>,
        alpha: f32,
        push: &mut dyn FnMut(SolidColorRenderElement),
    ) {
        let shade = Color::new_unpremul(0., 0.035, 0.025, 0.17 * alpha);
        let height = vertical.end - vertical.start;
        if height <= 0. {
            return;
        }

        let mut buffers = self.outside_view.borrow_mut();
        for (buffer, range) in buffers.iter_mut().zip([
            strip.start..visible.start,
            visible.end..strip.end,
        ]) {
            let width = range.end - range.start;
            if width <= 0. {
                continue;
            }
            buffer.update((width, height), shade);
            push(SolidColorRenderElement::from_buffer(
                buffer,
                (range.start, vertical.start),
                1.,
                Kind::Unspecified,
            ));
        }
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

fn caption(index: usize, name: Option<&str>, empty: bool) -> String {
    let name: String = name
        .unwrap_or_default()
        .chars()
        .take(128)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let name = name.trim();
    match (name.is_empty(), empty) {
        (true, true) => format!("{index}   Empty workspace"),
        (true, false) => format!("Workspace {index}"),
        (false, _) => format!("{index}   {name}"),
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
    let max_width = f64::from(key.width);
    let (width, height) = if key.thumbnail {
        (max_width.min(4096.), 22.)
    } else {
        (
            (f64::from(label.pixel_size().0) + 2. * PADDING)
                .max(30.)
                .min(max_width),
            HEIGHT,
        )
    };
    let pixel_width = (width * key.scale).ceil() as i32;
    let pixel_height = (height * key.scale).ceil() as i32;

    let surface = ImageSurface::create(cairo::Format::ARgb32, pixel_width, pixel_height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    let radius = if key.thumbnail { 7. } else { height / 2. };
    cr.arc(
        radius,
        radius,
        radius,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.arc(
        width - radius,
        radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        0.,
    );
    if key.thumbnail {
        cr.line_to(width, height);
        cr.line_to(0., height);
    } else {
        cr.arc(
            width - radius,
            height - radius,
            radius,
            0.,
            std::f64::consts::FRAC_PI_2,
        );
        cr.arc(
            radius,
            height - radius,
            radius,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        );
    }
    cr.close_path();
    let color = key.background;
    cr.set_source_rgba(
        color.r.into(),
        color.g.into(),
        color.b.into(),
        color.a.into(),
    );
    cr.fill()?;

    let status = key.status.filter(|_| width >= 178.).map(|text| {
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&FontDescription::from_string("sans-serif Medium 11px")));
        layout.set_text(text);
        let (width, height) = layout.pixel_size();
        (layout, f64::from(width), f64::from(height))
    });
    let reserved = status.as_ref().map_or(0., |(_, width, _)| width + PADDING);
    let label = layout(&cr, key);
    label.set_width(((width - 2. * PADDING - reserved) * f64::from(pango::SCALE)) as i32);
    let (_, h) = label.pixel_size();
    let ink = key.ink;
    cr.move_to(PADDING, (height - f64::from(h)) / 2.);
    cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
    pangocairo::functions::show_layout(&cr, &label);
    if let Some((status, w, h)) = status {
        cr.move_to(width - PADDING - w, (height - h) / 2.);
        cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.83);
        pangocairo::functions::show_layout(&cr, &status);
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
        assert_eq!(caption(2, None, false), "Workspace 2");
        assert_eq!(caption(3, Some("  "), false), "Workspace 3");
        assert_eq!(caption(4, Some("Field\nnotes"), false), "4   Field notes");
        assert_eq!(caption(1, Some(&"x".repeat(160)), false).len(), 132);
    }
}
