use std::time::Instant;

use niri_config::Config;
use niri_ipc::{ColumnDisplay, Placement, SizeChange, WorkspaceReferenceArg};
use smithay::wayland::xdg_activation::XdgActivationToken;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::utils::with_toplevel_role;
use crate::window::spawn_placement::SPAWN_PLACEMENT_TIMEOUT;
use crate::window::SpawnPlacement;

#[derive(Clone, Copy, PartialEq)]
enum Present {
    /// The window never presents an activation token.
    Never,
    /// The window presents the token before its initial commit.
    BeforeInitial,
    /// The window presents the token after the initial configure, before mapping.
    AfterInitial,
}

#[derive(Debug, PartialEq)]
struct Placed {
    workspace: String,
    /// Column index starting from 1, or `None` for floating windows.
    column: Option<usize>,
    display: Option<ColumnDisplay>,
    floating: bool,
    /// Width of the last configure the window received.
    width: i32,
}

fn set_up() -> (Fixture, ClientId) {
    let config = Config::parse_mem(
        r#"
        workspace "one"
        workspace "two"
        "#,
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    (f, id)
}

fn open(
    f: &mut Fixture,
    id: ClientId,
    title: &str,
    token: Option<&XdgActivationToken>,
    present: Present,
) -> WlSurface {
    let client = f.client(id);
    let window = client.create_window();
    let surface = window.surface.clone();
    window.set_title(title);
    if present == Present::BeforeInitial {
        client.state.activate(token.unwrap(), &surface);
    }
    client.window(&surface).commit();
    f.roundtrip(id);

    if present == Present::AfterInitial {
        f.client(id).state.activate(token.unwrap(), &surface);
        f.double_roundtrip(id);
    }

    // Like a real client, take the configured size, picking our own where it was left open.
    let window = f.client(id).window(&surface);
    let (w, h) = window.configures_received.last().unwrap().1.size;
    let size = |x: i32| {
        if x > 0 {
            u16::try_from(x).unwrap()
        } else {
            100
        }
    };
    window.set_size(size(w), size(h));
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

fn placed(f: &mut Fixture, id: ClientId, surface: &WlSurface, title: &str) -> Placed {
    let width = f
        .client(id)
        .window(surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size
        .0;

    let has_title = |mapped: &crate::window::Mapped| {
        with_toplevel_role(mapped.toplevel(), |role| {
            role.title.as_deref() == Some(title)
        })
    };

    let niri = f.niri();
    let (ws_idx, ws, mapped) = niri
        .layout
        .workspaces()
        .find_map(|(_, ws_idx, ws)| {
            ws.windows()
                .find(|mapped| has_title(mapped))
                .map(|mapped| (ws_idx, ws, mapped))
        })
        .unwrap();

    let column = ws
        .scrolling()
        .columns()
        .enumerate()
        .find(|(_, col)| col.tiles().any(|(tile, _)| has_title(tile.window())));

    Placed {
        workspace: ws
            .name()
            .cloned()
            .unwrap_or_else(|| format!("unnamed {}", ws_idx + 1)),
        column: column.as_ref().map(|(idx, _)| idx + 1),
        display: column.map(|(_, col)| col.display_mode()),
        floating: mapped.is_floating(),
        width,
    }
}

fn tiled(workspace: &str, column: usize, width: i32) -> Placed {
    Placed {
        workspace: String::from(workspace),
        column: Some(column),
        display: Some(ColumnDisplay::Normal),
        floating: false,
        width,
    }
}

fn workspace(name: &str) -> Option<WorkspaceReferenceArg> {
    Some(WorkspaceReferenceArg::Name(String::from(name)))
}

#[test]
fn opens_on_requested_workspace_and_column() {
    let (mut f, id) = set_up();
    let a = open(&mut f, id, "a", None, Present::Never);
    let b = open(&mut f, id, "b", None, Present::Never);
    assert_eq!(placed(&mut f, id, &a, "a"), tiled("one", 1, 936));
    assert_eq!(placed(&mut f, id, &b, "b"), tiled("one", 2, 936));

    let token = f.niri().register_spawn_placement(Placement {
        workspace: workspace("one"),
        column_index: Some(1),
        width: Some(SizeChange::SetProportion(25.)),
        ..Default::default()
    });
    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);

    assert_eq!(placed(&mut f, id, &c, "c"), tiled("one", 1, 460));
    assert_eq!(placed(&mut f, id, &a, "a"), tiled("one", 2, 936));
    assert_eq!(placed(&mut f, id, &b, "b"), tiled("one", 3, 936));
    assert!(f.niri().spawn_placements.is_empty());
}

#[test]
fn opens_on_inactive_workspace() {
    let (mut f, id) = set_up();
    let a = open(&mut f, id, "a", None, Present::Never);

    let token = f.niri().register_spawn_placement(Placement {
        workspace: workspace("two"),
        width: Some(SizeChange::SetFixed(800)),
        ..Default::default()
    });
    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);

    assert_eq!(placed(&mut f, id, &c, "c"), tiled("two", 1, 800));
    assert_eq!(placed(&mut f, id, &a, "a"), tiled("one", 1, 936));
}

#[test]
fn column_past_end_appends() {
    let (mut f, id) = set_up();
    open(&mut f, id, "a", None, Present::Never);
    open(&mut f, id, "b", None, Present::Never);
    // Focus the first column so that a normal window would open in the middle.
    f.niri().layout.focus_column_first();

    let token = f.niri().register_spawn_placement(Placement {
        column_index: Some(10),
        ..Default::default()
    });
    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);

    // Without a workspace, the column applies to the workspace the window normally opens on.
    assert_eq!(placed(&mut f, id, &c, "c"), tiled("one", 3, 936));
}

#[test]
fn missing_workspace_falls_back() {
    let (mut f, id) = set_up();
    open(&mut f, id, "a", None, Present::Never);

    let token = f.niri().register_spawn_placement(Placement {
        workspace: workspace("missing"),
        width: Some(SizeChange::SetFixed(800)),
        display: Some(ColumnDisplay::Tabbed),
        ..Default::default()
    });
    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);

    // The other fields still apply.
    let tabbed = Placed {
        display: Some(ColumnDisplay::Tabbed),
        ..tiled("one", 2, 800)
    };
    assert_eq!(placed(&mut f, id, &c, "c"), tabbed);
}

#[test]
fn unrelated_windows_are_unaffected() {
    let (mut f, id) = set_up();

    let token = f.niri().register_spawn_placement(Placement {
        workspace: workspace("two"),
        column_index: Some(1),
        width: Some(SizeChange::SetProportion(25.)),
        ..Default::default()
    });

    // A window without the token opens normally and leaves the placement pending.
    let plain = open(&mut f, id, "plain", None, Present::Never);
    assert_eq!(placed(&mut f, id, &plain, "plain"), tiled("one", 1, 936));
    assert_eq!(f.niri().spawn_placements.len(), 1);

    // A window presenting another valid token opens normally too.
    let (other, _) = f.niri().activation_state.create_external_token(None);
    let other = other.clone();
    let foreign = open(&mut f, id, "foreign", Some(&other), Present::BeforeInitial);
    assert_eq!(
        placed(&mut f, id, &foreign, "foreign"),
        tiled("one", 2, 936)
    );
    assert_eq!(f.niri().spawn_placements.len(), 1);

    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);
    assert_eq!(placed(&mut f, id, &c, "c"), tiled("two", 1, 460));
}

#[test]
fn only_first_window_is_placed() {
    let (mut f, id) = set_up();

    let token = f.niri().register_spawn_placement(Placement {
        workspace: workspace("two"),
        column_index: Some(1),
        width: Some(SizeChange::SetProportion(25.)),
        ..Default::default()
    });
    let first = open(&mut f, id, "first", Some(&token), Present::BeforeInitial);
    let second = open(&mut f, id, "second", Some(&token), Present::BeforeInitial);

    assert_eq!(placed(&mut f, id, &first, "first"), tiled("two", 1, 460));
    // The token was consumed, so the second window opens normally: after the focused first
    // window and with the default width.
    assert_eq!(placed(&mut f, id, &second, "second"), tiled("two", 2, 936));
}

#[test]
fn expired_placement_is_ignored() {
    let (mut f, id) = set_up();

    let Some(created) = Instant::now().checked_sub(SPAWN_PLACEMENT_TIMEOUT) else {
        // The monotonic clock is too young to backdate a placement.
        return;
    };
    let niri = f.niri();
    let (token, _) = niri.activation_state.create_external_token(None);
    let token = token.clone();
    let two = niri
        .layout
        .find_workspace_by_name("two")
        .map(|(_, ws)| ws.id());
    let placement = SpawnPlacement::new(
        Placement {
            column_index: Some(1),
            width: Some(SizeChange::SetProportion(25.)),
            ..Default::default()
        },
        two,
    );
    niri.spawn_placements
        .insert(token.clone(), placement, created);

    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);
    assert_eq!(placed(&mut f, id, &c, "c"), tiled("one", 1, 936));
    assert!(f.niri().spawn_placements.is_empty());
}

#[test]
fn placement_after_initial_configure() {
    let (mut f, id) = set_up();
    open(&mut f, id, "a", None, Present::Never);

    let token = f.niri().register_spawn_placement(Placement {
        workspace: workspace("two"),
        width: Some(SizeChange::SetProportion(25.)),
        ..Default::default()
    });
    let c = open(&mut f, id, "c", Some(&token), Present::AfterInitial);

    assert_eq!(placed(&mut f, id, &c, "c"), tiled("two", 1, 460));
}

#[test]
fn placement_can_open_floating() {
    let (mut f, id) = set_up();
    open(&mut f, id, "a", None, Present::Never);

    let token = f.niri().register_spawn_placement(Placement {
        column_index: Some(1),
        width: Some(SizeChange::SetFixed(500)),
        floating: Some(true),
        ..Default::default()
    });
    let c = open(&mut f, id, "c", Some(&token), Present::BeforeInitial);

    // The column index is ignored for floating windows.
    let floating = Placed {
        workspace: String::from("one"),
        column: None,
        display: None,
        floating: true,
        width: 500,
    };
    assert_eq!(placed(&mut f, id, &c, "c"), floating);
}
