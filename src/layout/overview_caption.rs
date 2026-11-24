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
use super::shadow::Shadow;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

const HEIGHT: f64 = 28.;
const PADDING: f64 = 10.;
pub(super) const SCROLL_BUTTON_HEIGHT: f64 = 32.;
pub(super) const SCROLL_CARD_HEIGHT: f64 = 176.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LabelKind {
    Caption,
    GroupHeading,
    StripButton {
        direction: Option<bool>,
        hidden: bool,
    },
    Preview {
        show_action: bool,
    },
    Button {
        align_right: bool,
    },
    WindowNavigation {
        left: bool,
        offset: usize,
        more: usize,
    },
    EdgeFade {
        height: u32,
        from_left: bool,
    },
}

#[derive(Debug)]
pub(super) struct OverviewCaption {
    title: PreviewLabel,
    pub(super) desktop_labels: [PreviewLabel; 3],
    desktop_groups: [RefCell<BorderRenderElement>; 3],
    frame: RefCell<[BorderRenderElement; 2]>,
    preview_hover: RefCell<BorderRenderElement>,
    fallback_fill: RefCell<SolidColorBuffer>,
    pub(super) shadow: RefCell<Shadow>,
    pub(super) group_shadows: [RefCell<Shadow>; 3],
}

impl Default for OverviewCaption {
    fn default() -> Self {
        Self {
            title: PreviewLabel::default(),
            desktop_labels: Default::default(),
            desktop_groups: std::array::from_fn(|_| RefCell::new(BorderRenderElement::empty())),
            frame: RefCell::new(std::array::from_fn(|_| BorderRenderElement::empty())),
            preview_hover: RefCell::new(BorderRenderElement::empty()),
            fallback_fill: RefCell::default(),
            shadow: RefCell::new(Shadow::new(niri_config::Shadow::default())),
            group_shadows: std::array::from_fn(|_| {
                RefCell::new(Shadow::new(niri_config::Shadow::default()))
            }),
        }
    }
}

#[derive(Debug, PartialEq)]
struct Key {
    renderer: ContextId<GlesTexture>,
    text: String,
    background: Color,
    ink: Color,
    kind: LabelKind,
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
        background: Color,
        kind: LabelKind,
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
            background,
            ink,
            kind,
            width: match kind {
                LabelKind::Caption => (width as u32 / 8) * 8,
                _ => width as u32,
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

pub(super) fn tint_caption(background: Color, accent: Color) -> Color {
    let mix = |base, highlight| base * 0.76 + highlight * 0.24;
    Color::new_unpremul(
        mix(background.r, accent.r),
        mix(background.g, accent.g),
        mix(background.b, accent.b),
        1.,
    )
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
            background,
            LabelKind::Caption,
            width,
            scale,
            location,
            alpha,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_desktop_group(
        &self,
        renderer: &mut impl NiriRenderer,
        index: usize,
        rect: Rectangle<f64, Logical>,
        emphasized: bool,
        backdrop: Color,
        accent: Color,
        alpha: f32,
        scale: f64,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        if rect.size.w < 12.
            || rect.size.h < 12.
            || alpha == 0.
            || !BorderRenderElement::has_shader(renderer)
        {
            return;
        }
        let surface = caption_background(accent, backdrop, false);
        let tint = if emphasized { 0.18 } else { 0. };
        let mix = |base, highlight| base * (1. - tint) + highlight * tint;
        let opacity = if is_light(backdrop) { 0.74 } else { 0.82 };
        let color = Color::new_unpremul(
            mix(surface.r, accent.r),
            mix(surface.g, accent.g),
            mix(surface.b, accent.b),
            alpha * opacity,
        );
        let geometry = Rectangle::from_size(rect.size);
        let mut group = self.desktop_groups[index].borrow_mut();
        group.update(
            rect.size,
            geometry,
            Default::default(),
            color,
            color,
            0.,
            geometry,
            0.,
            frame_radius(rect, 1.),
            scale as f32,
            1.,
        );
        push(group.clone().with_location(rect.loc).into());
    }

    pub(super) fn render_preview_hover(
        &self,
        renderer: &mut impl NiriRenderer,
        rect: Rectangle<f64, Logical>,
        color: Color,
        scale: f64,
    ) -> Option<FocusRingRenderElement> {
        if !BorderRenderElement::has_shader(renderer) {
            return None;
        }
        let geometry = Rectangle::from_size(rect.size);
        let mut border = self.preview_hover.borrow_mut();
        border.update(
            rect.size,
            geometry,
            Default::default(),
            color,
            color,
            0.,
            geometry,
            3.,
            CornerRadius::from(9.),
            scale as f32,
            1.,
        );
        Some(border.clone().with_location(rect.loc).into())
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
        let outline_alpha = if active { 0.22 } else { 0.32 };
        let outline = Color::new_unpremul(ink, ink, ink, outline_alpha * progress);
        let geo = Rectangle::from_size(rect.size);
        let radius = frame_radius(rect, progress);
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

pub(super) fn frame_radius(rect: Rectangle<f64, Logical>, progress: f32) -> CornerRadius {
    CornerRadius::from(18. * progress).fit_to(rect.size.w as f32, rect.size.h as f32)
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
    layout.set_ellipsize(
        if matches!(
            key.kind,
            LabelKind::StripButton { .. }
                | LabelKind::Button { .. }
                | LabelKind::WindowNavigation { .. }
        ) {
            EllipsizeMode::Middle
        } else {
            EllipsizeMode::End
        },
    );
    layout.set_text(&key.text);
    layout
}

fn rasterize(renderer: &mut GlesRenderer, key: &Key) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let probe = ImageSurface::create(cairo::Format::ARgb32, 1, 1)?;
    let cr = cairo::Context::new(&probe)?;
    cr.scale(key.scale, key.scale);
    let label = layout(&cr, key);
    let max_width = f64::from(key.width);
    let (width, height) = match key.kind {
        LabelKind::GroupHeading => (max_width.min(4096.), HEIGHT),
        LabelKind::Preview { .. } => (max_width.min(4096.), 22.),
        LabelKind::StripButton { .. } | LabelKind::Button { .. } => {
            (max_width, SCROLL_BUTTON_HEIGHT)
        }
        LabelKind::WindowNavigation { .. } => (max_width, SCROLL_CARD_HEIGHT),
        LabelKind::EdgeFade { height, .. } => (max_width, f64::from(height)),
        LabelKind::Caption => (
            (f64::from(label.pixel_size().0) + 2. * PADDING)
                .max(30.)
                .min(max_width),
            HEIGHT,
        ),
    };
    let pixel_width = (width * key.scale).ceil() as i32;
    let pixel_height = (height * key.scale).ceil() as i32;

    let surface = ImageSurface::create(cairo::Format::ARgb32, pixel_width, pixel_height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    if let LabelKind::EdgeFade { from_left, .. } = key.kind {
        let (from, to) = if from_left { (0., width) } else { (width, 0.) };
        let gradient = cairo::LinearGradient::new(from, 0., to, 0.);
        let color = key.background;
        for (stop, alpha) in [(0., 0.97), (0.45, 0.55), (1., 0.)] {
            gradient.add_color_stop_rgba(
                stop,
                color.r.into(),
                color.g.into(),
                color.b.into(),
                alpha,
            );
        }
        cr.set_source(&gradient)?;
        cr.paint()?;
        drop(cr);
        return upload_texture(renderer, surface, pixel_width, pixel_height, key.scale);
    }
    let radius = match key.kind {
        LabelKind::Preview { .. } => 7.,
        LabelKind::WindowNavigation { .. } => 14.,
        _ => height / 2.,
    };
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
    if matches!(key.kind, LabelKind::Preview { .. }) {
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
    let ink = key.ink;
    if key.kind == LabelKind::GroupHeading {
        cr.new_path();
    } else {
        cr.set_source_rgba(
            color.r.into(),
            color.g.into(),
            color.b.into(),
            color.a.into(),
        );
        if matches!(
            key.kind,
            LabelKind::StripButton { .. }
                | LabelKind::Button { .. }
                | LabelKind::WindowNavigation { .. }
        ) {
            cr.fill_preserve()?;
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.28);
            cr.set_line_width(1.);
            cr.stroke()?;
        } else {
            cr.fill()?;
        }
    }

    if let LabelKind::WindowNavigation { left, offset, more } = key.kind {
        let title = layout(&cr, key);
        title.set_single_paragraph_mode(false);
        title.set_wrap(pango::WrapMode::WordChar);
        title.set_height(-2);
        title.set_width(((width - 2. * PADDING) * f64::from(pango::SCALE)) as i32);
        let align = |w: i32| {
            if left {
                PADDING
            } else {
                width - PADDING - f64::from(w)
            }
        };
        cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        cr.move_to(align(title.pixel_size().0), 34.);
        pangocairo::functions::show_layout(&cr, &title);

        let heading = layout(&cr, key);
        heading.set_font_description(Some(&FontDescription::from_string("sans-serif 12px")));
        let action = match (offset, left) {
            (1, true) => "Previous window",
            (1, false) => "Next window",
            (_, true) => "Two windows back",
            (_, false) => "Two windows ahead",
        };
        let text = match (left, more) {
            (true, 0) => format!("←  {action}"),
            (false, 0) => format!("{action}  →"),
            (true, more) => format!("←  {action} · +{more}"),
            (false, more) => format!("+{more} · {action}  →"),
        };
        heading.set_width(((width - 2. * PADDING) * f64::from(pango::SCALE)) as i32);
        heading.set_text(&text);
        cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.76);
        cr.move_to(align(heading.pixel_size().0), 12.);
        pangocairo::functions::show_layout(&cr, &heading);
    } else {
        cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        let label = layout(&cr, key);
        let mut title_width = width - 2. * PADDING;
        let mut title_x = PADDING;
        if let LabelKind::StripButton {
            direction: Some(left),
            ..
        } = key.kind
        {
            let arrow = layout(&cr, key);
            arrow.set_font_description(Some(&FontDescription::from_string(
                "sans-serif Medium 18px",
            )));
            arrow.set_text(if left { "←" } else { "→" });
            let (w, h) = arrow.pixel_size();
            let x = if left {
                PADDING
            } else {
                width - PADDING - f64::from(w)
            };
            cr.move_to(x, (height - f64::from(h)) / 2.);
            pangocairo::functions::show_layout(&cr, &arrow);
            let slot = f64::from(w) + 5.;
            title_width -= slot;
            if left {
                title_x += slot;
            }
        }
        if matches!(key.kind, LabelKind::StripButton { hidden: true, .. }) {
            let status = layout(&cr, key);
            status.set_font_description(Some(&FontDescription::from_string("sans-serif 12px")));
            status.set_text("Hidden");
            let (w, h) = status.pixel_size();
            title_width -= f64::from(w) + PADDING;
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.78);
            cr.move_to(width - PADDING - f64::from(w), (height - f64::from(h)) / 2.);
            pangocairo::functions::show_layout(&cr, &status);
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        }
        if matches!(key.kind, LabelKind::Preview { show_action: true }) {
            let action = layout(&cr, key);
            action.set_font_description(Some(&FontDescription::from_string(
                "sans-serif Medium 12px",
            )));
            action.set_text("Show window");
            let (w, h) = action.pixel_size();
            let radius = 9.;
            let right = width - 3.;
            let left = right - f64::from(w) - 14.;
            title_width -= f64::from(w) + 15.;
            cr.arc(
                right - radius,
                height / 2.,
                radius,
                -std::f64::consts::FRAC_PI_2,
                std::f64::consts::FRAC_PI_2,
            );
            cr.arc(
                left + radius,
                height / 2.,
                radius,
                std::f64::consts::FRAC_PI_2,
                1.5 * std::f64::consts::PI,
            );
            cr.close_path();
            cr.fill()?;
            cr.set_source_rgb(color.r.into(), color.g.into(), color.b.into());
            cr.move_to(left + 7., (height - f64::from(h)) / 2.);
            pangocairo::functions::show_layout(&cr, &action);
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        }
        label.set_width((title_width * f64::from(pango::SCALE)) as i32);
        let (w, h) = label.pixel_size();
        let x = match key.kind {
            LabelKind::GroupHeading => (width - f64::from(w)) / 2.,
            LabelKind::StripButton { .. } => title_x + (title_width - f64::from(w)) / 2.,
            LabelKind::Button { align_right: true } => width - PADDING - f64::from(w),
            _ => PADDING,
        };
        cr.move_to(x, (height - f64::from(h)) / 2.);
        pangocairo::functions::show_layout(&cr, &label);
    }
    drop(cr);

    upload_texture(renderer, surface, pixel_width, pixel_height, key.scale)
}

fn upload_texture(
    renderer: &mut GlesRenderer,
    surface: ImageSurface,
    pixel_width: i32,
    pixel_height: i32,
    scale: f64,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    Ok(TextureBuffer::from_memory(
        renderer,
        &surface.take_data()?,
        Fourcc::Argb8888,
        (pixel_width, pixel_height),
        false,
        scale,
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
