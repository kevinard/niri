use niri_config::Config;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::layout::LayoutElement as _;
use crate::window_preview::{fit_size, CaptureError, PreviewPixels};

const RED: [u32; 4] = [u32::MAX, 0, 0, u32::MAX];

fn set_up(config: Config, renderer: bool) -> (Fixture, ClientId) {
    let mut f = Fixture::with_config(config);
    if renderer {
        f.niri_state().backend.headless().add_renderer().unwrap();
    }
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    (f, id)
}

fn create_window(f: &mut Fixture, id: ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_color_buffer(RED);
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    surface
}

fn only_window_id(f: &mut Fixture) -> u64 {
    let mut windows = f.niri().layout.windows();
    let (_, mapped) = windows.next().unwrap();
    assert!(windows.next().is_none());
    mapped.id().get()
}

fn assert_all_red(preview: &PreviewPixels) {
    for (i, px) in preview.pixels.chunks_exact(4).enumerate() {
        assert_eq!(px, [255, 0, 0, 255], "pixel {i}");
    }
}

#[test]
fn egl_capture_scales_window_down() {
    let (mut f, id) = set_up(Config::default(), true);
    create_window(&mut f, id, 400, 200);
    let window_id = only_window_id(&mut f);

    let size = f.niri().layout.windows().next().unwrap().1.size();
    let expected = fit_size(size.to_physical(1), 100);
    assert!(expected.w <= 100 && expected.h <= 100);
    assert!(expected.w == 100 || expected.h == 100);

    let preview = f
        .niri_state()
        .capture_window_preview(window_id, 100)
        .unwrap();
    assert_eq!(
        (preview.width as i32, preview.height as i32),
        (expected.w, expected.h)
    );
    assert_all_red(&preview);
}

#[test]
fn egl_capture_does_not_upscale_and_clamps() {
    let (mut f, id) = set_up(Config::default(), true);
    create_window(&mut f, id, 400, 200);
    let window_id = only_window_id(&mut f);
    let size = f.niri().layout.windows().next().unwrap().1.size();

    // Larger than the window: returned at its own size, even past the 512 clamp.
    let preview = f
        .niri_state()
        .capture_window_preview(window_id, 10000)
        .unwrap();
    let expected = fit_size(size.to_physical(1), 512);
    assert_eq!(
        (preview.width as i32, preview.height as i32),
        (expected.w, expected.h)
    );
    assert_all_red(&preview);

    // Below the minimum: clamped to 16.
    let preview = f.niri_state().capture_window_preview(window_id, 1).unwrap();
    assert_eq!(preview.width.max(preview.height), 16);
    assert_all_red(&preview);
}

#[test]
fn egl_capture_window_on_hidden_workspace() {
    let (mut f, id) = set_up(Config::default(), true);
    create_window(&mut f, id, 300, 300);
    let window_id = only_window_id(&mut f);

    // Move the window to the next workspace without following it.
    f.niri().layout.move_to_workspace_down(false);
    f.double_roundtrip(id);
    let active = f.niri().layout.active_workspace().unwrap();
    assert!(!active.windows().any(|w| w.id().get() == window_id));

    let preview = f
        .niri_state()
        .capture_window_preview(window_id, 64)
        .unwrap();
    assert!(preview.width.max(preview.height) <= 64);
    assert_all_red(&preview);
}

#[test]
fn capture_unknown_window() {
    let (mut f, _id) = set_up(Config::default(), false);
    let err = f
        .niri_state()
        .capture_window_preview(u64::MAX, 64)
        .unwrap_err();
    assert_eq!(err, CaptureError::UnknownWindow(u64::MAX));
}

#[test]
fn capture_disabled_by_config() {
    let config = Config::parse_mem("window-previews { off; }").unwrap();
    assert!(config.window_previews.off);

    let (mut f, id) = set_up(config, false);
    create_window(&mut f, id, 100, 100);
    let window_id = only_window_id(&mut f);

    let err = f
        .niri_state()
        .capture_window_preview(window_id, 64)
        .unwrap_err();
    assert_eq!(err, CaptureError::Disabled);
}
