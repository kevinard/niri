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
pub(super) const STRIP_BUTTON_HEIGHT: f64 = 40.;
pub(super) const STRIP_GROUP_BUTTON_HEIGHT: f64 = 28.;
pub(super) const STRIP_STATUS_HEIGHT: f64 = 20.;
pub(super) const STRIP_TRAY_HEIGHT: f64 = 48.;
pub(super) const STRIP_TRAY_PADDING: f64 = 6.;
pub(super) const SCROLL_CARD_HEIGHT: f64 = 176.;
pub(super) const OVERVIEW_CONTROL_HEIGHT: f64 = 32.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupStyle {
    Desktop,
    Offscreen,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FocusMarker {
    None,
    Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LabelKind {
    Caption,
    NavigationHint {
        tab: Option<&'static str>,
    },
    GroupTitle,
    OverviewCommand,
    StripTray,
    StripStatus,
    StripRevealGroup,
    ScrollWindow {
        left: bool,
        hovered: bool,
    },
    StripButton {
        status: Option<&'static str>,
        outlined: bool,
        hovered: bool,
        focus: FocusMarker,
        compact: bool,
    },
    LayerAction {
        show: bool,
    },
    Preview {
        badge: Option<&'static str>,
        current: bool,
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
    group_rules: [RefCell<SolidColorBuffer>; 3],
    group_backings: [RefCell<BorderRenderElement>; 3],
    selection: RefCell<BorderRenderElement>,
    frame: RefCell<[BorderRenderElement; 2]>,
    fallback_fill: RefCell<SolidColorBuffer>,
    pub(super) shadow: RefCell<Shadow>,
}

impl Default for OverviewCaption {
    fn default() -> Self {
        Self {
            title: PreviewLabel::default(),
            desktop_labels: Default::default(),
            group_rules: Default::default(),
            group_backings: std::array::from_fn(|_| RefCell::new(BorderRenderElement::empty())),
            selection: RefCell::new(BorderRenderElement::empty()),
            frame: RefCell::new(std::array::from_fn(|_| BorderRenderElement::empty())),
            fallback_fill: RefCell::default(),
            shadow: RefCell::new(Shadow::new(niri_config::Shadow::default())),
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
        let ink = caption_ink(background);
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

pub(super) fn caption_ink(background: Color) -> Color {
    if is_light(background) {
        Color::new_unpremul(0.06, 0.13, 0.11, 1.)
    } else {
        Color::new_unpremul(0.95, 0.98, 0.95, 1.)
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
    pub(super) fn render_group_rule(
        &self,
        index: usize,
        rect: Rectangle<f64, Logical>,
        style: GroupStyle,
        backdrop: Color,
        accent: Color,
        alpha: f32,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        let ink = if is_light(backdrop) { 0.16 } else { 0.88 };
        let color = match style {
            GroupStyle::Desktop => Color {
                a: alpha * 0.72,
                ..accent
            },
            GroupStyle::Offscreen => Color::new_unpremul(ink, ink, ink, alpha * 0.43),
            GroupStyle::Hidden => Color::new_unpremul(ink, ink, ink, alpha * 0.31),
        };
        let mut rule = self.group_rules[index].borrow_mut();
        rule.update(rect.size, color);
        push(SolidColorRenderElement::from_buffer(&rule, rect.loc, 1., Kind::Unspecified).into());
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_group_backing(
        &self,
        renderer: &mut impl NiriRenderer,
        index: usize,
        rect: Rectangle<f64, Logical>,
        style: GroupStyle,
        backdrop: Color,
        alpha: f32,
        scale: f64,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        if !BorderRenderElement::has_shader(renderer) {
            return;
        }
        let strength = match style {
            GroupStyle::Desktop => 0.80,
            GroupStyle::Offscreen => 0.52,
            GroupStyle::Hidden => 0.38,
        };
        let color = Color {
            a: alpha * strength,
            ..backdrop
        };
        let geometry = Rectangle::from_size(rect.size);
        let mut group = self.group_backings[index].borrow_mut();
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

    pub(super) fn render_selection(
        &self,
        renderer: &mut impl NiriRenderer,
        rect: Rectangle<f64, Logical>,
        accent: Color,
        alpha: f32,
        scale: f64,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        if !BorderRenderElement::has_shader(renderer) {
            return;
        }
        let geometry = Rectangle::from_size(rect.size);
        let mut selection = self.selection.borrow_mut();
        selection.update(
            rect.size,
            geometry,
            Default::default(),
            accent,
            accent,
            0.,
            geometry,
            3.,
            CornerRadius::from(10.).fit_to(rect.size.w as f32, rect.size.h as f32),
            scale as f32,
            alpha,
        );
        push(selection.clone().with_location(rect.loc).into());
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
    let font = if key.kind == LabelKind::StripRevealGroup {
        "sans-serif Bold 11px"
    } else if key.kind == LabelKind::StripStatus {
        "sans-serif Bold 13px"
    } else if matches!(key.kind, LabelKind::StripButton { .. })
        || matches!(key.kind, LabelKind::Preview { .. }) && key.width < 220
    {
        "sans-serif Medium 13px"
    } else {
        "sans-serif Medium 14px"
    };
    layout.set_font_description(Some(&FontDescription::from_string(font)));
    layout.set_single_paragraph_mode(true);
    layout.set_ellipsize(
        if matches!(
            key.kind,
            LabelKind::StripButton { .. }
                | LabelKind::Preview { .. }
                | LabelKind::OverviewCommand
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
        LabelKind::GroupTitle => (max_width.min(4096.), HEIGHT),
        LabelKind::NavigationHint { .. } | LabelKind::OverviewCommand => {
            (max_width.min(4096.), OVERVIEW_CONTROL_HEIGHT)
        }
        LabelKind::Preview { .. } => (max_width.min(4096.), 22.),
        LabelKind::StripTray => (max_width, STRIP_TRAY_HEIGHT),
        LabelKind::StripStatus => (max_width, STRIP_STATUS_HEIGHT),
        LabelKind::StripRevealGroup => (max_width, STRIP_BUTTON_HEIGHT + 2.),
        LabelKind::StripButton { compact: true, .. } => (max_width, STRIP_GROUP_BUTTON_HEIGHT),
        LabelKind::StripButton { .. }
        | LabelKind::ScrollWindow { .. }
        | LabelKind::LayerAction { .. } => (max_width, STRIP_BUTTON_HEIGHT),
        LabelKind::Button { .. } => (max_width, SCROLL_BUTTON_HEIGHT),
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
        LabelKind::OverviewCommand | LabelKind::NavigationHint { .. } => 9.,
        LabelKind::StripTray | LabelKind::WindowNavigation { .. } => 14.,
        LabelKind::StripButton { .. }
        | LabelKind::ScrollWindow { .. }
        | LabelKind::StripRevealGroup => 9.,
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
    if key.kind == LabelKind::GroupTitle {
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
            LabelKind::StripTray
                | LabelKind::ScrollWindow { .. }
                | LabelKind::OverviewCommand
                | LabelKind::NavigationHint { .. }
                | LabelKind::StripButton { outlined: true, .. }
                | LabelKind::LayerAction { .. }
                | LabelKind::Button { .. }
                | LabelKind::WindowNavigation { .. }
        ) {
            cr.fill_preserve()?;
            let stroke_alpha = match key.kind {
                LabelKind::StripTray => 0.14,
                LabelKind::ScrollWindow { hovered: true, .. }
                | LabelKind::StripButton { hovered: true, .. } => 1.,
                LabelKind::ScrollWindow { .. } => 0.54,
                _ => 0.28,
            };
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), stroke_alpha);
            cr.set_line_width(match key.kind {
                LabelKind::ScrollWindow { hovered: true, .. }
                | LabelKind::StripButton { hovered: true, .. } => 2.,
                LabelKind::ScrollWindow { .. } => 1.5,
                _ => 1.,
            });
            cr.stroke()?;
        } else {
            cr.fill()?;
        }
    }

    if key.kind == LabelKind::StripTray {
        drop(cr);
        return upload_texture(renderer, surface, pixel_width, pixel_height, key.scale);
    }

    if let LabelKind::NavigationHint { tab } = key.kind {
        let keycap = layout(&cr, key);
        keycap.set_font_description(Some(&FontDescription::from_string(
            "sans-serif Medium 12px",
        )));
        let detail = layout(&cr, key);
        detail.set_font_description(Some(&FontDescription::from_string(
            "sans-serif Medium 13px",
        )));
        let hints = [
            Some(("← →", key.text.as_str())),
            tab.map(|text| ("Tab", text)),
        ];
        let widths = hints.map(|hint| {
            hint.map(|(key, text)| {
                keycap.set_text(key);
                detail.set_text(text);
                f64::from(keycap.pixel_size().0 + detail.pixel_size().0) + 18.
            })
        });
        let total =
            widths.into_iter().flatten().sum::<f64>() + if tab.is_some() { 12. } else { 0. };
        let mut x = ((width - total) / 2.).max(6.);
        for (hint, hint_width) in hints.into_iter().zip(widths) {
            let (Some((key, text)), Some(hint_width)) = (hint, hint_width) else {
                continue;
            };
            keycap.set_text(key);
            detail.set_text(text);
            let (key_width, key_height) = keycap.pixel_size();
            let (_, text_height) = detail.pixel_size();
            let key_width = f64::from(key_width);
            cr.rectangle(x, 5., key_width + 12., height - 10.);
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.17);
            cr.fill()?;
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            cr.move_to(x + 6., (height - f64::from(key_height)) / 2.);
            pangocairo::functions::show_layout(&cr, &keycap);
            cr.move_to(x + key_width + 18., (height - f64::from(text_height)) / 2.);
            pangocairo::functions::show_layout(&cr, &detail);
            x += hint_width + 12.;
        }
    } else if let LabelKind::ScrollWindow { left, .. } = key.kind {
        let standalone = key.text.is_empty();
        let arrow_x = if standalone {
            width / 2.
        } else if left {
            14.
        } else {
            width - 14.
        };
        let arrow_y = height / 2.;
        let sign = if left { -1. } else { 1. };
        if standalone {
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            cr.set_line_width(2.);
        } else {
            cr.arc(arrow_x, arrow_y, 9., 0., std::f64::consts::TAU);
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.94);
            cr.fill()?;
            cr.set_source_rgb(color.r.into(), color.g.into(), color.b.into());
            cr.set_line_width(1.6);
        }
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_join(cairo::LineJoin::Round);
        let span = if standalone { 7. } else { 4. };
        cr.move_to(arrow_x - span * sign, arrow_y);
        cr.line_to(arrow_x + span * sign, arrow_y);
        cr.move_to(arrow_x, arrow_y - span * 0.875);
        cr.line_to(arrow_x + span * sign, arrow_y);
        cr.line_to(arrow_x, arrow_y + span * 0.875);
        cr.stroke()?;

        if standalone {
            drop(cr);
            return upload_texture(renderer, surface, pixel_width, pixel_height, key.scale);
        }

        let label = layout(&cr, key);
        label.set_ellipsize(EllipsizeMode::Middle);
        label.set_width(((width - 29. - PADDING) * f64::from(pango::SCALE)) as i32);
        label.set_alignment(if left {
            pango::Alignment::Left
        } else {
            pango::Alignment::Right
        });
        let x = if left { 29. } else { PADDING };
        let (title, detail) = key.text.split_once('\n').unwrap_or((&key.text, ""));
        label.set_text(title);
        label.set_font_description(Some(&FontDescription::from_string("sans-serif Bold 13px")));
        cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        cr.move_to(x, 4.);
        pangocairo::functions::show_layout(&cr, &label);
        label.set_text(detail);
        label.set_font_description(Some(&FontDescription::from_string(
            "sans-serif Medium 12px",
        )));
        label.set_ellipsize(EllipsizeMode::End);
        cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.88);
        cr.move_to(x, 20.);
        pangocairo::functions::show_layout(&cr, &label);
    } else if let LabelKind::WindowNavigation { left, offset, more } = key.kind {
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
        if key.kind == LabelKind::OverviewCommand {
            let keycap = layout(&cr, key);
            keycap.set_text("Enter");
            keycap.set_font_description(Some(&FontDescription::from_string(
                "sans-serif Medium 12px",
            )));
            let (w, h) = keycap.pixel_size();
            let key_width = f64::from(w) + 12.;
            let left = width - key_width - 6.;
            cr.rectangle(left, 5., key_width, height - 10.);
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.17);
            cr.fill()?;
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            cr.move_to(left + 6., (height - f64::from(h)) / 2.);
            pangocairo::functions::show_layout(&cr, &keycap);
            title_width = left - title_x - 6.;
        }
        if let LabelKind::LayerAction { show } = key.kind {
            let x = 13.5;
            let y = height / 2. - 6.5;
            cr.set_line_width(1.25);
            cr.rectangle(x, y + 4., 11., 8.);
            cr.stroke()?;
            cr.rectangle(x + 4., y, 11., 8.);
            cr.set_source_rgb(color.r.into(), color.g.into(), color.b.into());
            cr.fill_preserve()?;
            let front_alpha = if show { 0.62 } else { 1. };
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), front_alpha);
            cr.stroke()?;
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            title_x = 37.;
            title_width = width - title_x - PADDING;
            label.set_font_description(Some(&FontDescription::from_string(
                "sans-serif Medium 13px",
            )));
        }
        let (status, focus) = match key.kind {
            LabelKind::StripButton { status, focus, .. } => (status, focus),
            _ => (None, FocusMarker::None),
        };
        if focus != FocusMarker::None {
            label.set_font_description(Some(&FontDescription::from_string("sans-serif Bold 13px")));
            title_width -= 14.;
            title_x += 14.;
        }
        if let Some(status) = status {
            let detail = layout(&cr, key);
            detail.set_font_description(Some(&FontDescription::from_string("sans-serif 13px")));
            detail.set_text(status);
            detail.set_ellipsize(EllipsizeMode::End);
            detail.set_width((title_width * f64::from(pango::SCALE)) as i32);
            let (w, h) = detail.pixel_size();
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.93);
            cr.move_to(
                title_x + (title_width - f64::from(w)) / 2.,
                height - f64::from(h) - 4.,
            );
            pangocairo::functions::show_layout(&cr, &detail);
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        }
        if let LabelKind::Preview {
            badge: Some(badge),
            current,
        } = key.kind
        {
            let action = layout(&cr, key);
            action
                .set_font_description(Some(&FontDescription::from_string("sans-serif Bold 12px")));
            action.set_text(badge);
            let (w, h) = action.pixel_size();
            let dot_space = if current { 11. } else { 0. };
            let control_width = (f64::from(w) + 14. + dot_space).max(22.);
            let right = width - 4.;
            let left = right - control_width;
            title_width -= control_width;
            cr.arc(
                right - 9.,
                height / 2.,
                9.,
                -std::f64::consts::FRAC_PI_2,
                std::f64::consts::FRAC_PI_2,
            );
            cr.arc(
                left + 9.,
                height / 2.,
                9.,
                std::f64::consts::FRAC_PI_2,
                1.5 * std::f64::consts::PI,
            );
            cr.close_path();
            let fill_alpha = if current { 0.14 } else { 0.26 };
            let border_alpha = if current { 0.82 } else { 0.59 };
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), fill_alpha);
            cr.fill_preserve()?;
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), border_alpha);
            cr.set_line_width(1.);
            cr.stroke()?;
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            if current {
                cr.arc(left + 9., height / 2., 3.5, 0., std::f64::consts::TAU);
                cr.fill()?;
            }
            cr.move_to(
                left + (control_width - f64::from(w) + dot_space) / 2.,
                (height - f64::from(h)) / 2.,
            );
            pangocairo::functions::show_layout(&cr, &action);
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        }
        label.set_width((title_width * f64::from(pango::SCALE)) as i32);
        let (w, h) = label.pixel_size();
        let x = match key.kind {
            LabelKind::StripButton { .. }
            | LabelKind::StripStatus
            | LabelKind::StripRevealGroup => title_x + (title_width - f64::from(w)) / 2.,
            LabelKind::LayerAction { .. } => title_x,
            LabelKind::Button { align_right: true } => width - PADDING - f64::from(w),
            _ => PADDING,
        };
        let y = if key.kind == LabelKind::StripRevealGroup {
            height - f64::from(h) - 2.
        } else if status.is_some() {
            3.
        } else {
            (height - f64::from(h)) / 2.
        };
        if focus != FocusMarker::None {
            cr.arc(
                x - 9.,
                y + f64::from(h) / 2.,
                3.5,
                0.,
                std::f64::consts::TAU,
            );
            cr.fill()?;
        }
        cr.move_to(x, y);
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
