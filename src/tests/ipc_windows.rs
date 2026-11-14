use client::ClientId;
use smithay::reexports::wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;
use crate::ipc::server::ipc_parent_id;
use crate::utils::with_toplevel_role;

fn create_window(f: &mut Fixture, id: ClientId, title: &str) -> (WlSurface, XdgToplevel) {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    let toplevel = window.xdg_toplevel.clone();
    window.set_title(title);
    window.commit();
    f.roundtrip(id);
    (surface, toplevel)
}

fn map_window(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    let window = f.client(id).window(surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
}

/// Returns the IPC ids of the window and its parent, looking the window up by title.
fn window_and_parent_ids(f: &mut Fixture, title: &str) -> (u64, Option<u64>) {
    let layout = &f.niri().layout;
    let (_, mapped) = layout
        .windows()
        .find(|(_, mapped)| {
            with_toplevel_role(mapped.toplevel(), |role| {
                role.title.as_deref() == Some(title)
            })
        })
        .unwrap();
    (mapped.id().get(), ipc_parent_id(layout, mapped))
}

fn window_id(f: &mut Fixture, title: &str) -> u64 {
    window_and_parent_ids(f, title).0
}

fn parent_id(f: &mut Fixture, title: &str) -> Option<u64> {
    window_and_parent_ids(f, title).1
}

#[test]
fn parent_id_follows_set_parent() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let id = f.add_client();

    let (parent_surface, parent) = create_window(&mut f, id, "parent");
    map_window(&mut f, id, &parent_surface);
    let (child_surface, _) = create_window(&mut f, id, "child");
    map_window(&mut f, id, &child_surface);

    let parent_window_id = window_id(&mut f, "parent");
    assert_eq!(parent_id(&mut f, "parent"), None);
    assert_eq!(parent_id(&mut f, "child"), None);

    f.client(id)
        .window(&child_surface)
        .set_parent(Some(&parent));
    f.roundtrip(id);
    assert_eq!(parent_id(&mut f, "child"), Some(parent_window_id));
    assert_eq!(parent_id(&mut f, "parent"), None);

    f.client(id).window(&child_surface).set_parent(None);
    f.roundtrip(id);
    assert_eq!(parent_id(&mut f, "child"), None);
}

#[test]
fn parent_id_is_unset_until_parent_is_mapped() {
    let mut f = Fixture::new();
    f.add_output(1, (1280, 720));
    let id = f.add_client();

    let (parent_surface, parent) = create_window(&mut f, id, "parent");
    let (child_surface, _) = create_window(&mut f, id, "child");
    f.client(id)
        .window(&child_surface)
        .set_parent(Some(&parent));
    map_window(&mut f, id, &child_surface);

    // The parent is not mapped yet, so it has no IPC id to report.
    assert_eq!(parent_id(&mut f, "child"), None);

    map_window(&mut f, id, &parent_surface);
    let parent_window_id = window_id(&mut f, "parent");
    assert_eq!(parent_id(&mut f, "child"), Some(parent_window_id));
}
