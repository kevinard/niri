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
pub(super) const STRIP_FORECAST_HEIGHT: f64 = 126.;
pub(super) const STRIP_TRAY_HEIGHT: f64 = 52.;
pub(super) const SCROLL_CARD_HEIGHT: f64 = 176.;
pub(super) const OVERVIEW_CONTROL_HEIGHT: f64 = 32.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupStyle {
    Desktop,
    Offscreen,
    Hidden,
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
    StripViewportLabel,
    StripForecast,
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
    /// The rounded surface behind the window bar, with its shadow.
    BarTray,
    /// One window in the window bar.
    BarPill {
        state: BarPillState,
        hovered: bool,
        floating: bool,
        /// Windows stacked in the column; more than one shows a count.
        stacked: u8,
        /// A window on screen takes the background's colour, rather than the ink, for its mark.
        tinted: bool,
    },
    /// A round icon button in the window bar.
    BarButton {
        icon: BarIcon,
        enabled: bool,
        hovered: bool,
    },
    /// The popover that previews a window from the window bar; the text is its title.
    BarCard {
        height: u32,
    },
    /// A one-line hint beside a control, sized to its text and aligned to one end.
    Tooltip {
        right_align: bool,
    },
    /// Dimming over a window that is partly out of view; rounded on the sides not clipped.
    Scrim {
        height: u32,
        round_left: bool,
        round_right: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarPillState {
    /// Scrolled out of view, fully or partly.
    Offscreen,
    /// Entirely on screen.
    Visible,
    /// The focused window.
    Focused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BarIcon {
    Left,
    Right,
    ShowFloating,
    HideFloating,
}

/// Height of a window in the window bar.
pub(super) const BAR_PILL_HEIGHT: f64 = 32.;
/// Height of the window bar surface.
pub(super) const BAR_TRAY_HEIGHT: f64 = 44.;
/// Space around the tray and card textures for their shadows.
pub(super) const BAR_SHADOW_MARGIN: f64 = 32.;
/// Height of the title row of a window bar preview.
pub(super) const BAR_CARD_TITLE_HEIGHT: f64 = 32.;
pub(super) const BAR_CARD_PADDING: f64 = 12.;

thread_local! {
    /// Font family of compositor captions, following the titlebar font.
    static FAMILY: RefCell<String> = RefCell::new("sans-serif".to_owned());
}

/// Uses the families of a Pango font description, such as the titlebar font, for captions.
pub(super) fn set_caption_font(font: &str) {
    let family = FontDescription::from_string(font)
        .family()
        .map(|family| family.to_string())
        .filter(|family| !family.trim().is_empty())
        .unwrap_or_else(|| "sans-serif".to_owned());
    FAMILY.with(|current| {
        if *current.borrow() != family {
            *current.borrow_mut() = family;
        }
    });
}

thread_local! {
    static BAR_WIDTHS: RefCell<std::collections::HashMap<(String, String, bool, u8), f64>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Natural width of a window bar entry: its title, an icon and a count of stacked windows.
pub(super) fn measure_bar_label(text: &str, icon: bool, floating: bool, stacked: usize) -> f64 {
    let stacked = stacked.min(99) as u8;
    let family = FAMILY.with(|family| family.borrow().clone());
    let key = (text.to_owned(), family, icon || floating, stacked);
    if let Some(width) = BAR_WIDTHS.with(|cache| cache.borrow().get(&key).copied()) {
        return width;
    }
    let measure = || -> Option<f64> {
        let surface = ImageSurface::create(cairo::Format::ARgb32, 1, 1).ok()?;
        let cr = cairo::Context::new(&surface).ok()?;
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&caption_font("Semibold", 13.)));
        layout.set_single_paragraph_mode(true);
        layout.set_text(text);
        let mut width = f64::from(layout.pixel_size().0) + 2. * BAR_PILL_PADDING;
        if icon || floating {
            width += BAR_ICON_SIZE + BAR_ICON_GAP;
        }
        if stacked > 1 {
            layout.set_font_description(Some(&caption_font("Bold", 11.)));
            layout.set_text(&stacked.to_string());
            width += f64::from(layout.pixel_size().0).max(7.) + 16.;
        }
        // Leave room for hinting differences at fractional scales.
        Some((width + 6.).ceil())
    };
    let width = measure().unwrap_or(160.);
    BAR_WIDTHS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(key, width);
    });
    width
}

/// Horizontal padding inside a window bar entry.
const BAR_PILL_PADDING: f64 = 12.;
const BAR_ICON_SIZE: f64 = 16.;
const BAR_ICON_GAP: f64 = 7.;

fn caption_font(weight: &str, size: f64) -> FontDescription {
    let family = FAMILY.with(|family| family.borrow().clone());
    FontDescription::from_string(&format!("{family} {weight} {size}px"))
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
    family: String,
    /// Application ID whose icon the label shows, if any.
    icon: String,
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
        self.render_with_ink(
            renderer,
            text,
            String::new(),
            background,
            ink,
            kind,
            width,
            scale,
            location,
            alpha,
        )
    }

    /// Like [`Self::render`], with an explicit text color for translucent backgrounds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_with_ink(
        &self,
        renderer: &mut GlesRenderer,
        text: String,
        icon: String,
        background: Color,
        ink: Color,
        kind: LabelKind,
        width: f64,
        scale: f64,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
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
            family: FAMILY.with(|family| family.borrow().clone()),
            icon,
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
        self.render_outline(renderer, rect, accent, 3., alpha, scale, push);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_outline(
        &self,
        renderer: &mut impl NiriRenderer,
        rect: Rectangle<f64, Logical>,
        accent: Color,
        width: f32,
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
            width,
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
        let light = is_light(background);
        // The workspace you are on is a panel; the others are lighter slots.
        let strength = match (light, active) {
            (true, true) => 0.16,
            (true, false) => 0.22,
            (false, true) => 0.36,
            (false, false) => 0.22,
        };
        let opacity = background.a + (1. - background.a) * progress * strength;
        // Light cards are near white, so they stay clean over a misty wallpaper.
        let base = if light {
            Color::new_unpremul(0.97, 0.98, 0.97, 1.)
        } else {
            background
        };
        let fill = Color::new_unpremul(base.r, base.g, base.b, opacity);
        if !BorderRenderElement::has_shader(renderer) {
            let mut fallback = self.fallback_fill.borrow_mut();
            fallback.update(rect.size, fill);
            push(
                SolidColorRenderElement::from_buffer(&fallback, rect.loc, 1., Kind::Unspecified)
                    .into(),
            );
            return;
        }

        // A muted sage edge in light, so the pale panel does not melt into the mist.
        let outline = if light {
            let alpha = if active { 0.9 } else { 0.6 };
            Color::new_unpremul(0.553, 0.620, 0.580, alpha * progress)
        } else {
            // Dark panels are set apart by their fill; the edge only catches the light.
            let alpha = if active { 0.10 } else { 0.16 };
            Color::new_unpremul(0.94, 0.94, 0.94, alpha * progress)
        };
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

pub(super) fn is_light(color: Color) -> bool {
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
        (true, true) => format!("Workspace {index} · Empty"),
        (true, false) => format!("Workspace {index}"),
        (false, _) => format!("{index} · {name}"),
    }
}

fn layout(cr: &cairo::Context, key: &Key) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    let font = if key.kind == LabelKind::StripViewportLabel {
        caption_font("Bold", 12.)
    } else if matches!(key.kind, LabelKind::Preview { .. }) && key.width < 220 {
        caption_font("Medium", 13.)
    } else {
        caption_font("Medium", 14.)
    };
    layout.set_font_description(Some(&font));
    layout.set_single_paragraph_mode(true);
    layout.set_ellipsize(
        if matches!(
            key.kind,
            LabelKind::Preview { .. }
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
    if matches!(
        key.kind,
        LabelKind::BarTray
            | LabelKind::BarPill { .. }
            | LabelKind::BarButton { .. }
            | LabelKind::BarCard { .. }
            | LabelKind::Scrim { .. }
            | LabelKind::Tooltip { .. }
    ) {
        return rasterize_bar(renderer, key);
    }
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
        LabelKind::StripViewportLabel => (max_width, 16.),
        LabelKind::StripForecast => (max_width, STRIP_FORECAST_HEIGHT),
        LabelKind::Button { .. } => (max_width, SCROLL_BUTTON_HEIGHT),
        LabelKind::WindowNavigation { .. } => (max_width, SCROLL_CARD_HEIGHT),
        LabelKind::EdgeFade { height, .. } => (max_width, f64::from(height)),
        LabelKind::BarTray
        | LabelKind::BarPill { .. }
        | LabelKind::BarButton { .. }
        | LabelKind::BarCard { .. }
        | LabelKind::Scrim { .. }
        | LabelKind::Tooltip { .. } => (max_width, BAR_PILL_HEIGHT),
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
        LabelKind::StripTray | LabelKind::StripForecast | LabelKind::WindowNavigation { .. } => 14.,
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
                | LabelKind::StripForecast
                | LabelKind::OverviewCommand
                | LabelKind::NavigationHint { .. }
                | LabelKind::Button { .. }
                | LabelKind::WindowNavigation { .. }
        ) {
            cr.fill_preserve()?;
            let stroke_alpha = match key.kind {
                LabelKind::StripTray => 0.14,
                _ => 0.28,
            };
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), stroke_alpha);
            cr.set_line_width(1.);
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
        keycap.set_font_description(Some(&caption_font("Medium", 12.)));
        let detail = layout(&cr, key);
        detail.set_font_description(Some(&caption_font("Medium", 13.)));
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
            let (text_width, text_height) = detail.pixel_size();
            let key_width = f64::from(key_width);
            // The action first and its key after, as in the other overview command.
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            cr.move_to(x, (height - f64::from(text_height)) / 2.);
            pangocairo::functions::show_layout(&cr, &detail);
            let key_x = x + f64::from(text_width) + 6.;
            cr.rectangle(key_x, 5., key_width + 12., height - 10.);
            cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.17);
            cr.fill()?;
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
            cr.move_to(key_x + 6., (height - f64::from(key_height)) / 2.);
            pangocairo::functions::show_layout(&cr, &keycap);
            x += hint_width + 12.;
        }
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
        heading.set_font_description(Some(&caption_font("Regular", 12.)));
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
            keycap.set_font_description(Some(&caption_font("Medium", 12.)));
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
        if matches!(key.kind, LabelKind::Preview { .. }) && !key.icon.is_empty() {
            let icon_size = 14.;
            if crate::app_icons::paint(
                &cr,
                &key.icon,
                PADDING - 2.,
                (height - icon_size) / 2.,
                icon_size,
                key.scale,
            ) {
                title_x = PADDING - 2. + icon_size + 6.;
                title_width -= icon_size + 4.;
            }
            cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        }
        if let LabelKind::Preview {
            badge: Some(badge),
            current,
        } = key.kind
        {
            let action = layout(&cr, key);
            action.set_font_description(Some(&caption_font("Bold", 12.)));
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
            LabelKind::StripViewportLabel => title_x + (title_width - f64::from(w)) / 2.,
            LabelKind::Preview { .. } => title_x,
            LabelKind::Button { align_right: true } => width - PADDING - f64::from(w),
            _ => PADDING,
        };
        let y = if key.kind == LabelKind::StripForecast {
            8.
        } else {
            (height - f64::from(h)) / 2.
        };
        cr.move_to(x, y);
        pangocairo::functions::show_layout(&cr, &label);
    }
    drop(cr);

    upload_texture(renderer, surface, pixel_width, pixel_height, key.scale)
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.).min(h / 2.).max(0.);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.);
    cr.arc(x + w - r, y + h - r, r, 0., std::f64::consts::FRAC_PI_2);
    cr.arc(
        x + r,
        y + h - r,
        r,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + r,
        y + r,
        r,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.close_path();
}

/// A soft shadow under a rounded rectangle, built from stacked translucent outlines.
#[allow(clippy::too_many_arguments)]
fn soft_shadow(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    r: f64,
    blur: f64,
    offset: f64,
    alpha: f64,
) -> Result<(), cairo::Error> {
    let steps = 12;
    for step in 0..steps {
        let t = f64::from(step) / f64::from(steps);
        let grow = blur * (1. - t);
        rounded_rect(
            cr,
            x - grow,
            y - grow + offset,
            w + 2. * grow,
            h + 2. * grow,
            r + grow,
        );
        // Inner layers accumulate, so the shadow is darkest under the surface.
        cr.set_source_rgba(0., 0., 0., alpha / f64::from(steps) * (0.35 + t));
        cr.fill()?;
    }
    Ok(())
}

fn set_source(cr: &cairo::Context, color: Color, alpha: f64) {
    cr.set_source_rgba(
        color.r.into(),
        color.g.into(),
        color.b.into(),
        f64::from(color.a) * alpha,
    );
}

fn rasterize_bar(
    renderer: &mut GlesRenderer,
    key: &Key,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let margin = BAR_SHADOW_MARGIN;
    let inner_width = f64::from(key.width);
    let (width, height) = match key.kind {
        LabelKind::BarTray => (inner_width + 2. * margin, BAR_TRAY_HEIGHT + 2. * margin),
        LabelKind::BarCard { height } => {
            (inner_width + 2. * margin, f64::from(height) + 2. * margin)
        }
        LabelKind::BarButton {
            icon: BarIcon::ShowFloating | BarIcon::HideFloating,
            ..
        } => (inner_width, BAR_PILL_HEIGHT),
        LabelKind::BarButton {
            icon: BarIcon::Left | BarIcon::Right,
            ..
        } if !key.text.is_empty() => (inner_width, BAR_PILL_HEIGHT),
        LabelKind::BarButton { .. } => (BAR_PILL_HEIGHT, BAR_PILL_HEIGHT),
        LabelKind::Scrim { height, .. } => (inner_width, f64::from(height)),
        _ => (inner_width, BAR_PILL_HEIGHT),
    };
    let pixel_width = (width * key.scale).ceil() as i32;
    let pixel_height = (height * key.scale).ceil() as i32;
    anyhow::ensure!(
        pixel_width > 0 && pixel_height > 0 && pixel_width <= 16384 && pixel_height <= 16384,
        "window bar texture out of bounds"
    );
    let surface = ImageSurface::create(cairo::Format::ARgb32, pixel_width, pixel_height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    let background = key.background;
    let ink = key.ink;
    let light = is_light(background);

    match key.kind {
        LabelKind::BarTray => {
            let h = BAR_TRAY_HEIGHT;
            let r = h / 2.;
            let shadow = if light { 0.16 } else { 0.42 };
            soft_shadow(&cr, margin, margin, inner_width, h, r, 14., 4., shadow)?;
            rounded_rect(&cr, margin, margin, inner_width, h, r);
            set_source(&cr, background, 1.);
            cr.fill_preserve()?;
            set_source(&cr, ink, if light { 0.10 } else { 0.12 });
            cr.set_line_width(1.);
            cr.stroke()?;
        }
        LabelKind::BarCard {
            height: card_height,
        } => {
            let h = f64::from(card_height);
            let r = 14.;
            let shadow = if light { 0.14 } else { 0.5 };
            soft_shadow(&cr, margin, margin, inner_width, h, r, 16., 6., shadow)?;
            rounded_rect(&cr, margin, margin, inner_width, h, r);
            set_source(&cr, background, 1.);
            cr.fill_preserve()?;
            set_source(&cr, ink, if light { 0.10 } else { 0.14 });
            cr.set_line_width(1.);
            cr.stroke()?;
            let layout = pangocairo::functions::create_layout(&cr);
            layout.set_font_description(Some(&caption_font("Medium", 13.)));
            layout.set_single_paragraph_mode(true);
            layout.set_ellipsize(EllipsizeMode::End);
            layout.set_text(&key.text);
            let padding = BAR_CARD_PADDING + 4.;
            let icon_size = 18.;
            let has_icon = crate::app_icons::paint(
                &cr,
                &key.icon,
                margin + padding,
                margin + (BAR_CARD_TITLE_HEIGHT - icon_size) / 2. + 1.,
                icon_size,
                key.scale,
            );
            let text_x = margin + padding + if has_icon { icon_size + 8. } else { 0. };
            layout.set_width(
                ((inner_width - (text_x - margin) - padding) * f64::from(pango::SCALE)) as i32,
            );
            let (_, text_height) = layout.pixel_size();
            set_source(&cr, ink, 1.);
            cr.move_to(
                text_x,
                margin + (BAR_CARD_TITLE_HEIGHT - f64::from(text_height)) / 2. + 1.,
            );
            pangocairo::functions::show_layout(&cr, &layout);
        }
        LabelKind::BarPill {
            state,
            hovered,
            floating,
            stacked,
            tinted,
        } => {
            let r = height / 2.;
            if background.a > 0. {
                rounded_rect(&cr, 0., 0., width, height, r);
                set_source(&cr, background, 1.);
                cr.fill()?;
            }
            // Focus is the accent fill and hover a neutral tint; a window on screen gets a short
            // mark under its name, so the three states never share a shape.
            if hovered && state != BarPillState::Focused {
                rounded_rect(&cr, 0., 0., width, height, r);
                set_source(&cr, ink, 0.16);
                cr.fill()?;
            }
            // A window on screen gets a faint accent tint and edge, the same colour as focus at a
            // fraction of its weight.
            if state == BarPillState::Visible && !hovered {
                let (tint, fill, edge) = if tinted {
                    (
                        Color {
                            a: 1.,
                            ..background
                        },
                        0.12,
                        0.4,
                    )
                } else {
                    (ink, 0.04, 0.26)
                };
                rounded_rect(&cr, 0.5, 0.5, width - 1., height - 1., r - 0.5);
                set_source(&cr, tint, fill);
                cr.fill_preserve()?;
                set_source(&cr, tint, edge);
                cr.set_line_width(1.);
                cr.stroke()?;
            }
            let text_alpha: f64 = match state {
                BarPillState::Offscreen if !hovered => 0.78,
                _ => 1.,
            };
            let layout = pangocairo::functions::create_layout(&cr);
            let weight = if state == BarPillState::Focused {
                "Semibold"
            } else {
                "Medium"
            };
            layout.set_font_description(Some(&caption_font(weight, 13.)));
            layout.set_single_paragraph_mode(true);
            layout.set_ellipsize(EllipsizeMode::End);
            layout.set_text(&key.text);
            let padding = BAR_PILL_PADDING;
            let icon_size = BAR_ICON_SIZE;
            let has_icon =
                crate::app_icons::icon(&key.icon, (icon_size * key.scale).round() as i32).is_some();
            let icon_space = if has_icon || floating {
                icon_size + BAR_ICON_GAP
            } else {
                0.
            };
            let badge = (stacked > 1).then(|| {
                let badge = pangocairo::functions::create_layout(&cr);
                badge.set_font_description(Some(&caption_font("Bold", 11.)));
                badge.set_text(&stacked.to_string());
                badge
            });
            let badge_width = badge
                .as_ref()
                .map_or(0., |badge| f64::from(badge.pixel_size().0).max(7.) + 10.);
            let badge_space = if badge.is_some() {
                badge_width + 6.
            } else {
                0.
            };
            let available = (width - 2. * padding - icon_space - badge_space).max(1.);
            layout.set_width((available * f64::from(pango::SCALE)) as i32);
            let (text_width, text_height) = layout.pixel_size();
            let content = f64::from(text_width) + icon_space + badge_space;
            let x = ((width - content) / 2.).max(padding);
            if let Some(badge) = &badge {
                let bx = x + icon_space + f64::from(text_width) + 6.;
                let bh = 16.;
                rounded_rect(&cr, bx, (height - bh) / 2., badge_width, bh, bh / 2.);
                set_source(&cr, ink, 0.16);
                cr.fill()?;
                let (bw, bth) = badge.pixel_size();
                set_source(&cr, ink, text_alpha.max(0.85));
                cr.move_to(
                    bx + (badge_width - f64::from(bw)) / 2.,
                    (height - f64::from(bth)) / 2.,
                );
                pangocairo::functions::show_layout(&cr, badge);
            }
            set_source(&cr, ink, text_alpha);
            if has_icon {
                let _ = cr.save();
                crate::app_icons::paint(
                    &cr,
                    &key.icon,
                    x,
                    (height - icon_size) / 2.,
                    icon_size,
                    key.scale,
                );
                let _ = cr.restore();
                if text_alpha < 1. {
                    // Offscreen windows recede, icon included.
                    rounded_rect(
                        &cr,
                        x - 1.,
                        (height - icon_size) / 2. - 1.,
                        icon_size + 2.,
                        icon_size + 2.,
                        4.,
                    );
                    cr.set_source_rgba(0., 0., 0., 1. - text_alpha);
                    cr.set_operator(cairo::Operator::DestOut);
                    cr.fill()?;
                    cr.set_operator(cairo::Operator::Over);
                }
                set_source(&cr, ink, text_alpha);
            } else if floating {
                // Two overlapping windows.
                let y = height / 2. - 5.5;
                cr.set_line_width(1.25);
                rounded_rect(&cr, x + 0.5, y + 3.5, 8., 7., 1.5);
                cr.stroke()?;
                rounded_rect(&cr, x + 3.5, y + 0.5, 8., 7., 1.5);
                cr.stroke()?;
            }
            cr.move_to(x + icon_space, (height - f64::from(text_height)) / 2.);
            pangocairo::functions::show_layout(&cr, &layout);
        }
        LabelKind::BarButton {
            icon,
            enabled,
            hovered,
        } => {
            let c = height / 2.;
            if enabled && hovered {
                rounded_rect(&cr, 0., 0., width, height, height / 2.);
                set_source(&cr, ink, 0.12);
                cr.fill()?;
            }
            set_source(&cr, ink, if enabled { 0.92 } else { 0.32 });
            cr.set_line_width(1.75);
            cr.set_line_cap(cairo::LineCap::Round);
            cr.set_line_join(cairo::LineJoin::Round);
            match icon {
                BarIcon::Left | BarIcon::Right => {
                    let left = icon == BarIcon::Left;
                    let dir = if left { -1. } else { 1. };
                    // The chevron sits at the outer end, the count of windows that way beside it.
                    let cx = if left { c } else { width - c };
                    let span = 4.5;
                    cr.move_to(cx - dir * span * 0.6, c - span);
                    cr.line_to(cx + dir * span * 0.6, c);
                    cr.line_to(cx - dir * span * 0.6, c + span);
                    cr.stroke()?;
                    // The windows that way, as a "+N" badge like the one counting a column's
                    // stacked windows.
                    if !key.text.is_empty() {
                        let count = pangocairo::functions::create_layout(&cr);
                        count.set_font_description(Some(&caption_font("Bold", 11.)));
                        count.set_text(&format!("+{}", key.text));
                        let (cw, ch) = count.pixel_size();
                        let bw = f64::from(cw).max(7.) + 10.;
                        let bh = 16.;
                        let bx = if left { cx + 8. } else { cx - 8. - bw };
                        rounded_rect(&cr, bx, (height - bh) / 2., bw, bh, bh / 2.);
                        set_source(&cr, ink, 0.16);
                        cr.fill()?;
                        set_source(&cr, ink, 0.92);
                        cr.move_to(
                            bx + (bw - f64::from(cw)) / 2.,
                            (height - f64::from(ch)) / 2.,
                        );
                        pangocairo::functions::show_layout(&cr, &count);
                    }
                }
                BarIcon::ShowFloating | BarIcon::HideFloating => {
                    // Two overlapping windows, alone or followed by the action's name.
                    let cx = if key.text.is_empty() {
                        width / 2. + 1.
                    } else {
                        BAR_PILL_PADDING + BAR_ICON_SIZE / 2.
                    };
                    cr.set_line_width(1.25);
                    rounded_rect(&cr, cx - 7., c - 2., 10., 8., 2.);
                    cr.stroke()?;
                    rounded_rect(&cr, cx - 3., c - 6., 10., 8., 2.);
                    if icon == BarIcon::HideFloating {
                        cr.fill()?;
                    } else {
                        cr.stroke()?;
                    }
                    let layout = pangocairo::functions::create_layout(&cr);
                    layout.set_font_description(Some(&caption_font("Medium", 13.)));
                    layout.set_single_paragraph_mode(true);
                    layout.set_text(&key.text);
                    let (_, text_h) = layout.pixel_size();
                    cr.move_to(
                        BAR_PILL_PADDING + BAR_ICON_SIZE + BAR_ICON_GAP,
                        (height - f64::from(text_h)) / 2.,
                    );
                    pangocairo::functions::show_layout(&cr, &layout);
                }
            }
        }
        LabelKind::Scrim {
            round_left,
            round_right,
            ..
        } => {
            let r = 16_f64.min(width / 2.).min(height / 2.);
            let (rl, rr) = (
                if round_left { r } else { 0. },
                if round_right { r } else { 0. },
            );
            cr.new_sub_path();
            cr.move_to(rl, 0.);
            cr.line_to(width - rr, 0.);
            if rr > 0. {
                cr.arc(width - rr, rr, rr, -std::f64::consts::FRAC_PI_2, 0.);
            }
            cr.line_to(width, height - rr);
            if rr > 0. {
                cr.arc(width - rr, height - rr, rr, 0., std::f64::consts::FRAC_PI_2);
            }
            cr.line_to(rl, height);
            if rl > 0. {
                cr.arc(
                    rl,
                    height - rl,
                    rl,
                    std::f64::consts::FRAC_PI_2,
                    std::f64::consts::PI,
                );
            }
            cr.line_to(0., rl);
            if rl > 0. {
                cr.arc(rl, rl, rl, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
            }
            cr.close_path();
            set_source(&cr, background, 1.);
            cr.fill()?;
        }
        LabelKind::Tooltip { right_align } => {
            let layout = pangocairo::functions::create_layout(&cr);
            layout.set_font_description(Some(&caption_font("Medium", 13.)));
            layout.set_single_paragraph_mode(true);
            layout.set_ellipsize(EllipsizeMode::End);
            layout.set_width(
                (((inner_width - 2. * 12. - 2.).max(1.)) * f64::from(pango::SCALE)) as i32,
            );
            layout.set_text(&key.text);
            let (text_w, text_h) = layout.pixel_size();
            let w = (f64::from(text_w) + 2. * 12.).min(inner_width - 2.);
            let h = height - 2.;
            let x = if right_align {
                inner_width - 1. - w
            } else {
                1.
            };
            rounded_rect(&cr, x, 1., w, h, h / 2.);
            set_source(&cr, background, 1.);
            cr.fill_preserve()?;
            set_source(&cr, ink, if light { 0.16 } else { 0.18 });
            cr.set_line_width(1.);
            cr.stroke()?;
            set_source(&cr, ink, 1.);
            cr.move_to(x + 12., (height - f64::from(text_h)) / 2.);
            pangocairo::functions::show_layout(&cr, &layout);
        }
        _ => {}
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
        assert_eq!(caption(2, None, true), "Workspace 2 · Empty");
        assert_eq!(caption(3, Some("  "), false), "Workspace 3");
        assert_eq!(caption(4, Some("Field\nnotes"), false), "4 · Field notes");
        assert_eq!(caption(1, Some(&"x".repeat(160)), false).len(), 133);
    }
}
