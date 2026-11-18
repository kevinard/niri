use super::*;
use super::client::ClientId;
use crate::layout::{LayoutElement, HitType, titlebar::{ButtonState, TitlebarPart}};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{Mode, ZxdgToplevelDecorationV1};
use wayland_client::protocol::{wl_pointer, wl_surface::WlSurface};

fn setup(mode: Mode) -> (Fixture, ClientId, WlSurface, ZxdgToplevelDecorationV1) {
    let config = niri_config::Config::parse_mem(
        "prefer-no-csd\nanimations { off; }\nhotkey-overlay { skip-at-startup; }\nlayout { titlebar { on; height 32; }; border { off; }; }\nwindow-rule { geometry-corner-radius 12; clip-to-geometry true; }"
    ).unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (800, 600));
    let id = f.add_client();
    let client = f.client(id);
    let manager = client.state.decoration_manager.clone().unwrap();
    let window = client.create_window();
    let surface = window.surface.clone();
    let decoration =
        manager.get_toplevel_decoration(&window.xdg_toplevel, &window.qh, surface.clone());
    decoration.set_mode(mode);
    window.commit();
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    assert_eq!(
        window.configures_received.last().unwrap().1.decoration_mode,
        Some(mode)
    );
    let configure = &window.configures_received.last().unwrap().1;
    let size = configure.size;
    assert_eq!(size.1, if mode == Mode::ServerSide { 536 } else { 568 });
    // Clients sizing themselves to the bounds must leave room for the titlebar too.
    assert_eq!(configure.bounds.unwrap().1, size.1);
    window.set_size(size.0 as u16, size.1 as u16);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    let client = f.client(id);
    client
        .state
        .seats
        .keys()
        .next()
        .unwrap()
        .get_pointer(&client.qh, ());
    f.double_roundtrip(id);
    (f, id, surface, decoration)
}

fn control(f: &mut Fixture, part: TitlebarPart) -> (u32, u32) {
    let niri = f.niri();
    for y in 0..100 {
        for x in 0..800 {
            let contents = niri.contents_under((x as f64 + 0.5, y as f64 + 0.5).into());
            if matches!(contents.window, Some((_, HitType::Titlebar { part: found })) if found == part)
            {
                return (x, y);
            }
        }
    }
    panic!("missing titlebar control {part:?}");
}

fn motion(f: &mut Fixture, id: ClientId, point: (u32, u32)) {
    let pointer = f.client(id).state.virtual_pointer();
    pointer.motion_absolute(0, point.0, point.1, 800, 600);
    pointer.frame();
    f.double_roundtrip(id);
}

fn button(f: &mut Fixture, id: ClientId, state: wl_pointer::ButtonState) {
    button_code(f, id, 0x110, state);
}

fn button_code(f: &mut Fixture, id: ClientId, code: u32, state: wl_pointer::ButtonState) {
    let pointer = f.client(id).state.virtual_pointer();
    pointer.button(0, code, state);
    pointer.frame();
    f.double_roundtrip(id);
}

fn click(f: &mut Fixture, id: ClientId) {
    button(f, id, wl_pointer::ButtonState::Pressed);
    button(f, id, wl_pointer::ButtonState::Released);
}

fn interaction(f: &mut Fixture) -> Option<(TitlebarPart, ButtonState)> {
    f.niri()
        .layout
        .windows()
        .next()
        .unwrap()
        .1
        .titlebar_interaction()
}

fn is_maximized(f: &mut Fixture, id: ClientId, surface: &WlSurface) -> bool {
    let window = f.client(id).window(surface);
    window
        .configures_received
        .last()
        .unwrap()
        .1
        .states
        .contains(
        &smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel::State::Maximized,
    )
}

#[test]
fn negotiated_mode_is_applied_with_the_client_commit() {
    let (mut f, id, surface, decoration) = setup(Mode::ServerSide);
    assert!(f.niri().layout.windows().next().unwrap().1.has_ssd());
    decoration.set_mode(Mode::ClientSide);
    f.double_roundtrip(id);
    assert!(f.niri().layout.windows().next().unwrap().1.has_ssd());
    // The configure carrying the new mode already requests the height without a titlebar.
    let window = f.client(id).window(&surface);
    let configure = &window.configures_received.last().unwrap().1;
    assert_eq!(configure.decoration_mode, Some(Mode::ClientSide));
    assert_eq!(configure.size.1, 568);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(!f.niri().layout.windows().next().unwrap().1.has_ssd());
    let niri = f.niri();
    niri.layout
        .with_windows(|_, _, _, layout| assert_eq!(layout.window_offset_in_tile.1, 0.));
}

#[test]
fn client_side_windows_keep_their_full_content_area() {
    let (mut f, _, _, _) = setup(Mode::ClientSide);
    f.niri().layout.with_windows(|_, _, _, layout| {
        assert_eq!(layout.window_offset_in_tile.1, 0.);
        assert_eq!(layout.tile_size.1, layout.window_size.1 as f64);
    });
}

#[test]
fn title_and_app_id_changes_redraw_without_a_surface_commit() {
    for mode in [Mode::ServerSide, Mode::ClientSide] {
        let (mut f, id, surface, _) = setup(mode);
        f.double_roundtrip(id);
        let output = f.niri_output(1);
        let sequence = |f: &mut Fixture| f.niri().output_state[&output].frame_callback_sequence;

        let before = sequence(&mut f);
        f.client(id)
            .window(&surface)
            .xdg_toplevel
            .set_app_id("org.canopa.Changed".into());
        f.double_roundtrip(id);
        let after_app_id = sequence(&mut f);
        assert_eq!(after_app_id != before, mode == Mode::ServerSide);

        f.client(id).window(&surface).set_title("A changed title");
        f.double_roundtrip(id);
        assert_eq!(sequence(&mut f) != after_app_id, mode == Mode::ServerSide);
    }
}

#[test]
fn close_only_fires_on_release_over_the_same_control() {
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    let close = control(&mut f, TitlebarPart::Close);
    motion(&mut f, id, close);
    button(&mut f, id, wl_pointer::ButtonState::Pressed);
    assert!(!f.client(id).window(&surface).close_requested);
    motion(&mut f, id, (0, 590));
    button(&mut f, id, wl_pointer::ButtonState::Released);
    assert!(!f.client(id).window(&surface).close_requested);
    motion(&mut f, id, close);
    button(&mut f, id, wl_pointer::ButtonState::Pressed);
    button(&mut f, id, wl_pointer::ButtonState::Released);
    assert!(f.client(id).window(&surface).close_requested);
    assert!(f.client(id).state.pointer_buttons_received.is_empty());
}

#[test]
fn maximize_control_requests_content_size_without_the_frame() {
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    let maximize = control(&mut f, TitlebarPart::Maximize);
    motion(&mut f, id, maximize);
    button(&mut f, id, wl_pointer::ButtonState::Pressed);
    button(&mut f, id, wl_pointer::ButtonState::Released);
    let window = f.client(id).window(&surface);
    let configure = &window.configures_received.last().unwrap().1;
    assert!(configure.states.contains(
        &smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel::State::Maximized
    ));
    assert_eq!(configure.size, (800, 568));
}

#[test]
fn dragging_a_floating_titlebar_moves_the_window_without_client_button_events() {
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    let window = f.niri().layout.windows().next().unwrap().1.window.clone();
    f.niri().layout.set_window_floating(Some(&window), true);
    f.double_roundtrip(id);
    f.niri_complete_animations();
    let position = |f: &mut Fixture| {
        let mut position = None;
        f.niri()
            .layout
            .with_windows(|_, _, _, layout| position = layout.tile_pos_in_workspace_view);
        position.unwrap()
    };
    let before = position(&mut f);
    let drag = control(&mut f, TitlebarPart::Drag);
    motion(&mut f, id, drag);
    button(&mut f, id, wl_pointer::ButtonState::Pressed);
    motion(&mut f, id, (drag.0 + 100, drag.1 + 60));
    button(&mut f, id, wl_pointer::ButtonState::Released);
    f.niri_complete_animations();
    assert_ne!(before, position(&mut f));
    assert!(!f.client(id).window(&surface).close_requested);
    assert!(f.client(id).state.pointer_buttons_received.is_empty());
}

#[test]
fn egl_titlebar_is_rendered_with_the_window() {
    use smithay::backend::allocator::Fourcc;
    use smithay::utils::Transform;

    use crate::render_helpers::{RenderCtx, RenderTarget};

    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    f.client(id)
        .window(&surface)
        .set_title("Canopa — A window title with <plain text> & controls");
    f.double_roundtrip(id);
    f.niri_state().backend.headless().add_renderer().unwrap();
    let output = f.niri_output(1);
    let point = control(&mut f, TitlebarPart::Drag);
    let state = f.niri_state();
    state.niri.update_render_elements(Some(&output));
    let pixels = state
        .backend
        .with_primary_renderer(|renderer| {
            let elements = state.niri.render_to_vec(
                RenderCtx {
                    renderer,
                    target: RenderTarget::Output,
                    xray: None,
                },
                &output,
                false,
            );
            crate::render_helpers::render_to_vec(
                renderer,
                (800, 600).into(),
                1.0.into(),
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )
            .unwrap()
        })
        .unwrap();
    let offset = ((point.1 + 8) * 800 + point.0 + 4) as usize * 4;
    assert_eq!(&pixels[offset..offset + 4], &[48, 48, 48, 255]);
    if let Some(directory) = std::env::var_os("NIRI_TITLEBAR_ARTIFACTS") {
        std::fs::create_dir_all(&directory).unwrap();
        let path = std::path::Path::new(&directory).join("ssd-titlebar.png");
        let file = std::fs::File::create(path).unwrap();
        crate::utils::write_png_rgba8(file, 800, 600, &pixels).unwrap();
    }
}

#[test]
fn other_buttons_during_a_titlebar_click_do_not_stick() {
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    let close = control(&mut f, TitlebarPart::Close);
    motion(&mut f, id, close);
    button(&mut f, id, wl_pointer::ButtonState::Pressed);
    button_code(&mut f, id, 0x111, wl_pointer::ButtonState::Pressed);
    button_code(&mut f, id, 0x111, wl_pointer::ButtonState::Released);
    button(&mut f, id, wl_pointer::ButtonState::Released);
    assert!(f.client(id).window(&surface).close_requested);
    assert!(f.client(id).state.pointer_buttons_received.is_empty());

    // A later click elsewhere starts the seat's implicit click grab, which ends only once
    // the seat sees no button held. A release that never reached the seat would keep it.
    motion(&mut f, id, (400, 300));
    click(&mut f, id);
    assert!(!f.niri().seat.get_pointer().unwrap().is_grabbed());
}

#[test]
fn hovering_and_pressing_controls_is_shown_and_the_cursor_is_reset() {
    let (mut f, id, _, _) = setup(Mode::ServerSide);
    let close = control(&mut f, TitlebarPart::Close);
    let drag = control(&mut f, TitlebarPart::Drag);
    f.niri()
        .cursor_manager
        .set_cursor_image(smithay::input::pointer::CursorImageStatus::Hidden);
    motion(&mut f, id, close);
    assert_eq!(
        interaction(&mut f),
        Some((TitlebarPart::Close, ButtonState::Hovered))
    );
    assert!(matches!(
        f.niri().cursor_manager.cursor_image(),
        smithay::input::pointer::CursorImageStatus::Named(_)
    ));
    button(&mut f, id, wl_pointer::ButtonState::Pressed);
    assert_eq!(
        interaction(&mut f),
        Some((TitlebarPart::Close, ButtonState::Pressed))
    );
    // Moving away while pressed shows the control released; nothing happens on release.
    motion(&mut f, id, drag);
    assert_eq!(interaction(&mut f), None);
    button(&mut f, id, wl_pointer::ButtonState::Released);
    motion(&mut f, id, (400, 300));
    assert_eq!(interaction(&mut f), None);
}

#[test]
fn double_clicking_the_titlebar_toggles_maximize() {
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    let drag = control(&mut f, TitlebarPart::Drag);
    motion(&mut f, id, drag);
    click(&mut f, id);
    assert!(!is_maximized(&mut f, id, &surface));
    click(&mut f, id);
    assert!(is_maximized(&mut f, id, &surface));
    assert!(f.client(id).state.pointer_buttons_received.is_empty());
}

#[test]
fn double_click_can_be_disabled() {
    let config = niri_config::Config::parse_mem(
        "prefer-no-csd\nanimations { off; }\nlayout { titlebar { on; double-click \"none\"; }; }",
    )
    .unwrap();
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    f.niri_state().reload_config(Ok(config));
    f.double_roundtrip(id);
    let drag = control(&mut f, TitlebarPart::Drag);
    motion(&mut f, id, drag);
    click(&mut f, id);
    click(&mut f, id);
    assert!(!is_maximized(&mut f, id, &surface));
}

#[test]
fn fixed_size_windows_have_no_maximize_control() {
    let (mut f, id, surface, _) = setup(Mode::ServerSide);
    {
        let window = f.client(id).window(&surface);
        window.xdg_toplevel.set_min_size(400, 300);
        window.xdg_toplevel.set_max_size(400, 300);
        window.commit();
    }
    f.double_roundtrip(id);
    let niri = f.niri();
    let found = (0..100)
        .flat_map(|y| (0..800).map(move |x| (x, y)))
        .any(|(x, y)| {
            matches!(
                niri.contents_under((x as f64 + 0.5, y as f64 + 0.5).into())
                    .window,
                Some((
                    _,
                    HitType::Titlebar {
                        part: TitlebarPart::Maximize
                    }
                ))
            )
        });
    assert!(!found);
    let drag = control(&mut f, TitlebarPart::Drag);
    motion(&mut f, id, drag);
    click(&mut f, id);
    click(&mut f, id);
    assert!(!is_maximized(&mut f, id, &surface));
}

#[test]
fn window_rule_disables_the_titlebar() {
    let config = niri_config::Config::parse_mem(
        "prefer-no-csd\nanimations { off; }\nhotkey-overlay { skip-at-startup; }\nlayout { titlebar { on; }; border { off; }; }\nwindow-rule { draw-titlebar false; }",
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (800, 600));
    let id = f.add_client();
    let client = f.client(id);
    let manager = client.state.decoration_manager.clone().unwrap();
    let window = client.create_window();
    let surface = window.surface.clone();
    let decoration =
        manager.get_toplevel_decoration(&window.xdg_toplevel, &window.qh, surface.clone());
    decoration.set_mode(Mode::ServerSide);
    window.commit();
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    let configure = &window.configures_received.last().unwrap().1;
    assert_eq!(configure.decoration_mode, Some(Mode::ServerSide));
    assert_eq!(configure.size.1, 568);
}
