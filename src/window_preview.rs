//! Small window thumbnails ("previews") for desktop shells.
//!
//! A preview is rendered from the window's own surface tree, independently of where (or whether)
//! the window is currently visible on an output. Borders, focus rings, shadows, rounded-corner
//! clipping and popups are not included.

use std::{iter, thread};

use anyhow::{bail, ensure, Context as _};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::ExportMem as _;
use smithay::utils::{Physical, Point, Scale, Size, Transform};

#[cfg(feature = "dbus")]
use crate::dbus::canopa_window_preview::WindowPreviewToNiri;
use crate::layout::LayoutElement as _;
use crate::niri::State;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::{
    render_and_download, render_to_texture, render_to_vec, RenderCtx, RenderTarget,
};
use crate::utils::write_png_rgba8;
use crate::window::Mapped;

/// Smallest accepted value for the requested maximum preview size.
pub const MIN_MAX_SIZE: u32 = 16;
/// Largest accepted value for the requested maximum preview size.
pub const MAX_MAX_SIZE: u32 = 512;

/// Largest side of the full-resolution intermediate render, in physical pixels.
///
/// Bounds GPU memory for very large windows; such windows are first rendered at a reduced scale.
const MAX_FIRST_PASS_SIZE: i32 = 4096;

/// Why a preview could not be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// Window previews are turned off in the config.
    Disabled,
    /// There is no mapped window with this id.
    UnknownWindow(u64),
    /// Rendering or encoding failed.
    Failed(String),
}

/// Rendered preview pixels.
#[derive(Debug)]
pub struct PreviewPixels {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA, 8 bits per channel, top row first.
    pub pixels: Vec<u8>,
}

/// An encoded preview.
#[derive(Debug)]
pub struct PreviewPng {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

/// Clamps a requested maximum preview size to the supported range.
pub fn clamp_max_size(max_size: u32) -> u32 {
    max_size.clamp(MIN_MAX_SIZE, MAX_MAX_SIZE)
}

/// Fits `size` into a `max_size`×`max_size` square preserving the aspect ratio.
///
/// Never upscales: a size that already fits is returned as is. Each side is at least 1.
pub fn fit_size(size: Size<i32, Physical>, max_size: u32) -> Size<i32, Physical> {
    let w = size.w.max(1);
    let h = size.h.max(1);
    let max = i32::try_from(max_size).unwrap_or(i32::MAX).max(1);

    if w <= max && h <= max {
        return Size::from((w, h));
    }

    let factor = f64::min(f64::from(max) / f64::from(w), f64::from(max) / f64::from(h));
    let fit = |x: i32| ((f64::from(x) * factor).round() as i32).clamp(1, max);
    Size::from((fit(w), fit(h)))
}

/// Returns the next size in the downscaling chain from `current` towards `target`.
///
/// Linear filtering only averages neighboring texels, so downscaling by more than 2× in one pass
/// skips source pixels and aliases. The chain halves the size until it is within 2× of the
/// target, then goes to the target in a final pass. Returns `None` once `current` is the target.
pub fn next_downscale_step(
    current: Size<i32, Physical>,
    target: Size<i32, Physical>,
) -> Option<Size<i32, Physical>> {
    if current == target {
        return None;
    }

    if current.w > target.w * 2 || current.h > target.h * 2 {
        let half = |cur: i32, tgt: i32| ((cur + 1) / 2).max(tgt);
        Some(Size::from((
            half(current.w, target.w),
            half(current.h, target.h),
        )))
    } else {
        Some(target)
    }
}

/// Converts premultiplied RGBA pixels to straight alpha in place, as PNG expects.
pub fn unpremultiply(pixels: &mut [u8]) {
    for px in pixels.chunks_exact_mut(4) {
        let a = u32::from(px[3]);
        match a {
            0 => px[..3].fill(0),
            255 => (),
            _ => {
                for c in &mut px[..3] {
                    *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
    }
}

/// Encodes rendered preview pixels as a PNG.
pub fn encode_png(mut preview: PreviewPixels) -> anyhow::Result<PreviewPng> {
    let _span = tracy_client::span!("window_preview::encode_png");

    unpremultiply(&mut preview.pixels);

    let mut png = Vec::new();
    write_png_rgba8(&mut png, preview.width, preview.height, &preview.pixels)
        .context("error encoding PNG")?;

    Ok(PreviewPng {
        width: preview.width,
        height: preview.height,
        png,
    })
}

/// Renders the window's surface tree scaled down to fit `max_size`.
///
/// `scale` is the scale of the output the window is on; it determines the window size in physical
/// pixels, which the preview never exceeds. `max_size` should already be clamped.
///
/// Renders as a screen capture, so `block-out-from` window rules apply. Window opacity rules are
/// ignored.
pub fn render_window_preview(
    renderer: &mut GlesRenderer,
    mapped: &Mapped,
    scale: f64,
    max_size: u32,
) -> anyhow::Result<PreviewPixels> {
    let _span = tracy_client::span!("window_preview::render_window_preview");

    let size = mapped.size().to_f64();
    ensure!(size.w > 0. && size.h > 0., "window has no size");

    let full = size.to_physical_precise_round(scale);
    let target = fit_size(full, max_size);

    // Render the first pass at full resolution, unless that is huge.
    let largest = f64::from(full.w.max(full.h).max(1));
    let first_scale = scale * f64::min(1., f64::from(MAX_FIRST_PASS_SIZE) / largest);
    let first_size: Size<i32, Physical> = size.to_physical_precise_round(first_scale);
    let first_size = Size::from((first_size.w.max(target.w), first_size.h.max(target.h)));

    let mut elements = Vec::new();
    let ctx = RenderCtx {
        renderer: &mut *renderer,
        target: RenderTarget::ScreenCapture,
        xray: None,
    };
    mapped.render_normal(
        ctx,
        Point::from((0., 0.)),
        Scale::from(first_scale),
        1.,
        &mut |elem| elements.push(elem),
    );
    if elements.is_empty() {
        bail!("window has no contents");
    }

    // Elements are pushed top to bottom; draw them bottom to top.
    let elements = elements.iter().rev();
    let fourcc = Fourcc::Abgr8888;

    let pixels = if first_size == target {
        render_to_vec(
            renderer,
            target,
            Scale::from(first_scale),
            Transform::Normal,
            fourcc,
            elements,
        )?
    } else {
        let (mut texture, _) = render_to_texture(
            renderer,
            first_size,
            Scale::from(first_scale),
            Transform::Normal,
            fourcc,
            elements,
        )?;

        let mut current = first_size;
        loop {
            let next = next_downscale_step(current, target).unwrap();
            let elem = texture_element(renderer, texture, next);

            if next == target {
                let mapping = render_and_download(
                    renderer,
                    next,
                    Scale::from(1.),
                    Transform::Normal,
                    fourcc,
                    iter::once(elem),
                )?;
                let copy = renderer
                    .map_texture(&mapping)
                    .context("error mapping texture")?;
                break copy.to_vec();
            }

            (texture, _) = render_to_texture(
                renderer,
                next,
                Scale::from(1.),
                Transform::Normal,
                fourcc,
                iter::once(elem),
            )?;
            current = next;
        }
    };

    let width = target.w as u32;
    let height = target.h as u32;
    ensure!(
        pixels.len() == width as usize * height as usize * 4,
        "unexpected pixel buffer size"
    );

    Ok(PreviewPixels {
        width,
        height,
        pixels,
    })
}

impl State {
    /// Renders a preview of the mapped window with the given IPC id.
    ///
    /// Works for windows on any workspace, visible or not, since it renders the window's own
    /// elements rather than reading an output framebuffer.
    pub fn capture_window_preview(
        &mut self,
        window_id: u64,
        max_size: u32,
    ) -> Result<PreviewPixels, CaptureError> {
        if self.niri.config.borrow().window_previews.off {
            return Err(CaptureError::Disabled);
        }

        let max_size = clamp_max_size(max_size);

        let mut windows = self.niri.layout.windows();
        let Some((monitor, mapped)) = windows.find(|(_, m)| m.id().get() == window_id) else {
            return Err(CaptureError::UnknownWindow(window_id));
        };
        let scale = monitor.map_or(1., |mon| mon.output().current_scale().fractional_scale());

        let res = self.backend.with_primary_renderer(|renderer| {
            render_window_preview(renderer, mapped, scale, max_size)
        });
        match res {
            Some(Ok(preview)) => Ok(preview),
            Some(Err(err)) => {
                warn!("error rendering window preview: {err:?}");
                Err(CaptureError::Failed(String::from("error rendering window")))
            }
            None => Err(CaptureError::Failed(String::from("no renderer"))),
        }
    }

    /// Handles a capture request from the window preview D-Bus service.
    ///
    /// Renders on the main loop, then encodes the PNG on a separate thread and replies from it.
    #[cfg(feature = "dbus")]
    pub fn on_window_preview_msg(&mut self, msg: WindowPreviewToNiri) {
        let WindowPreviewToNiri::Capture {
            window_id,
            max_size,
            reply,
        } = msg;
        let _span = tracy_client::span!("WindowPreview::Capture");

        let preview = match self.capture_window_preview(window_id, max_size) {
            Ok(preview) => preview,
            Err(err) => {
                // The caller may have gone away; nothing to do then.
                let _ = reply.try_send(Err(err));
                return;
            }
        };

        // If spawning fails, the reply sender is dropped and the caller gets an error.
        let res = thread::Builder::new()
            .name(String::from("Window Preview Encoder"))
            .spawn(move || {
                let res = encode_png(preview).map_err(|err| {
                    warn!("error encoding window preview: {err:?}");
                    CaptureError::Failed(String::from("error encoding preview"))
                });
                let _ = reply.try_send(res);
            });
        if let Err(err) = res {
            warn!("error spawning window preview encoder thread: {err:?}");
        }
    }
}

/// Creates an element drawing the whole texture stretched to `size` at scale 1.
fn texture_element(
    renderer: &GlesRenderer,
    texture: GlesTexture,
    size: Size<i32, Physical>,
) -> TextureRenderElement<GlesTexture> {
    let buffer = TextureBuffer::from_texture(renderer, texture, 1., Transform::Normal, Vec::new());
    TextureRenderElement::from_texture_buffer(
        buffer,
        Point::from((0., 0.)),
        1.,
        None,
        Some(size.to_logical(1).to_f64()),
        Kind::Unspecified,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(w: i32, h: i32) -> Size<i32, Physical> {
        Size::from((w, h))
    }

    #[test]
    fn clamps_max_size() {
        assert_eq!(clamp_max_size(0), 16);
        assert_eq!(clamp_max_size(15), 16);
        assert_eq!(clamp_max_size(16), 16);
        assert_eq!(clamp_max_size(256), 256);
        assert_eq!(clamp_max_size(512), 512);
        assert_eq!(clamp_max_size(513), 512);
        assert_eq!(clamp_max_size(u32::MAX), 512);
    }

    #[test]
    fn fit_preserves_aspect_ratio() {
        assert_eq!(fit_size(s(1920, 1080), 256), s(256, 144));
        assert_eq!(fit_size(s(1080, 1920), 256), s(144, 256));
        assert_eq!(fit_size(s(1000, 1000), 100), s(100, 100));
        assert_eq!(fit_size(s(1001, 333), 100), s(100, 33));
    }

    #[test]
    fn fit_never_upscales() {
        assert_eq!(fit_size(s(100, 50), 256), s(100, 50));
        assert_eq!(fit_size(s(256, 256), 256), s(256, 256));
        assert_eq!(fit_size(s(1, 1), 512), s(1, 1));
    }

    #[test]
    fn fit_keeps_sides_positive() {
        assert_eq!(fit_size(s(10000, 1), 16), s(16, 1));
        assert_eq!(fit_size(s(0, 0), 16), s(1, 1));
        assert_eq!(fit_size(s(0, 20), 16), s(1, 16));
    }

    #[test]
    fn downscale_chain_halves_then_finishes() {
        let mut chain = vec![];
        let mut cur = s(1920, 1080);
        let target = fit_size(cur, 128);
        while let Some(next) = next_downscale_step(cur, target) {
            chain.push(next);
            cur = next;
        }
        assert_eq!(
            chain,
            [s(960, 540), s(480, 270), s(240, 135), s(128, 72)],
            "{chain:?}"
        );
    }

    #[test]
    fn downscale_chain_steps_are_at_most_2x() {
        for (w, h, max) in [
            (1920, 1080, 16),
            (3001, 17, 64),
            (17, 3001, 512),
            (513, 512, 512),
        ] {
            let mut cur = s(w, h);
            let target = fit_size(cur, max);
            while let Some(next) = next_downscale_step(cur, target) {
                assert!(next.w >= target.w && next.h >= target.h);
                assert!(
                    next.w * 2 >= cur.w && next.h * 2 >= cur.h,
                    "{cur:?} -> {next:?}"
                );
                assert!(next.w <= cur.w && next.h <= cur.h);
                cur = next;
            }
            assert_eq!(cur, target);
        }
    }

    #[test]
    fn unpremultiplies() {
        let mut px = [0, 0, 0, 0, 10, 20, 30, 0, 255, 128, 0, 255, 64, 32, 0, 128];
        unpremultiply(&mut px);
        assert_eq!(
            px,
            [0, 0, 0, 0, 0, 0, 0, 0, 255, 128, 0, 255, 128, 64, 0, 128]
        );
    }

    #[test]
    fn encodes_png() {
        let preview = PreviewPixels {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 0, 0],
        };
        let png = encode_png(preview).unwrap();
        assert_eq!((png.width, png.height), (2, 1));
        assert_eq!(&png.png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
