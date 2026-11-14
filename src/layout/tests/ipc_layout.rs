use std::fmt::Write as _;

use insta::assert_snapshot;
use niri_config::animations::{Curve, EasingParams, Kind};

use super::*;

/// Formats the IPC tile positions of the active workspace, ordered by window id.
fn format_ipc_positions(layout: &Layout<TestWindow>) -> String {
    let mut buf = String::new();
    let ws = layout.active_workspace().unwrap();
    let mut tiles: Vec<_> = ws
        .tiles_with_ipc_layouts()
        .map(|(tile, layout)| (*tile.window().id(), layout))
        .collect();
    tiles.sort_by_key(|(id, _)| *id);
    for (id, layout) in tiles {
        let (x, y) = layout.tile_pos_in_workspace_view.unwrap();
        writeln!(&mut buf, "{id}: x:{x:>5.0} y:{y:>3.0}").unwrap();
    }
    buf
}

/// Formats the render positions of the active workspace, ordered by window id.
fn format_render_positions(layout: &Layout<TestWindow>) -> String {
    let mut buf = String::new();
    let ws = layout.active_workspace().unwrap();
    let mut tiles: Vec<_> = ws
        .tiles_with_render_positions()
        .map(|(tile, pos, _)| (*tile.window().id(), pos))
        .collect();
    tiles.sort_by_key(|(id, _)| *id);
    for (id, pos) in tiles {
        let Point { x, y, .. } = pos;
        writeln!(&mut buf, "{id}: x:{x:>5.0} y:{y:>3.0}").unwrap();
    }
    buf
}

fn make_options() -> Options {
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });

    let mut options = Options {
        layout: niri_config::Layout {
            gaps: 0.0,
            ..Default::default()
        },
        ..Options::default()
    };
    options.animations.horizontal_view_movement.0.kind = LINEAR;
    options.animations.window_movement.0.kind = LINEAR;
    options.animations.window_resize.anim.kind = LINEAR;

    options
}

/// Three 500 px wide columns on a 1280 px wide output, with the last one focused.
fn set_up_three_columns() -> Layout<TestWindow> {
    let mut ops = vec![Op::AddOutput(1)];
    for id in 1..=3 {
        ops.extend([
            Op::AddWindow {
                params: TestWindowParams::new(id),
            },
            Op::SetForcedSize {
                id,
                size: Some(Size::new(500, 300)),
            },
            Op::Communicate(id),
        ]);
    }
    ops.push(Op::CompleteAnimations);

    check_ops_with_options(make_options(), ops)
}

#[test]
fn scrolling_tiles_report_position_in_workspace_view() {
    let layout = set_up_three_columns();

    // The view is scrolled to show the last column, so the first column is partially off-screen
    // to the left.
    assert_snapshot!(format_ipc_positions(&layout), @r"
    1: x: -220 y:  0
    2: x:  280 y:  0
    3: x:  780 y:  0
    ");
    assert_eq!(
        format_ipc_positions(&layout),
        format_render_positions(&layout)
    );
}

#[test]
fn scrolling_tile_position_follows_view_scroll() {
    let mut layout = set_up_three_columns();

    check_ops_on_layout(&mut layout, [Op::FocusColumnFirst, Op::CompleteAnimations]);

    assert_snapshot!(format_ipc_positions(&layout), @r"
    1: x:    0 y:  0
    2: x:  500 y:  0
    3: x: 1000 y:  0
    ");
    assert_eq!(
        format_ipc_positions(&layout),
        format_render_positions(&layout)
    );
}

#[test]
fn scrolling_tile_position_reports_view_animation_target() {
    let mut layout = set_up_three_columns();

    check_ops_on_layout(&mut layout, [Op::FocusColumnFirst]);

    // The view animation has just started: tiles are still rendered at the old positions, but
    // IPC already reports the target positions.
    assert_snapshot!(format_render_positions(&layout), @r"
    1: x: -220 y:  0
    2: x:  280 y:  0
    3: x:  780 y:  0
    ");
    let target = format_ipc_positions(&layout);
    assert_snapshot!(target, @r"
    1: x:    0 y:  0
    2: x:  500 y:  0
    3: x: 1000 y:  0
    ");

    // Halfway through the animation, IPC positions do not change.
    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);
    assert_snapshot!(format_render_positions(&layout), @r"
    1: x: -110 y:  0
    2: x:  390 y:  0
    3: x:  890 y:  0
    ");
    assert_eq!(format_ipc_positions(&layout), target);

    Op::AdvanceAnimations { msec_delta: 500 }.apply(&mut layout);
    assert_eq!(format_ipc_positions(&layout), target);
    assert_eq!(format_render_positions(&layout), target);
}

#[test]
fn scrolling_tile_position_stays_put_during_view_gesture() {
    let mut layout = set_up_three_columns();
    let before = format_ipc_positions(&layout);

    check_ops_on_layout(
        &mut layout,
        [
            Op::ViewOffsetGestureBegin {
                output_idx: 1,
                workspace_idx: None,
                is_touchpad: false,
            },
            Op::ViewOffsetGestureUpdate {
                delta: -300.,
                timestamp: Duration::from_millis(10),
                is_touchpad: false,
            },
        ],
    );

    // The view moved with the gesture, but IPC keeps the pre-gesture positions.
    assert_ne!(format_render_positions(&layout), before);
    assert_eq!(format_ipc_positions(&layout), before);

    // When the gesture ends, IPC reports where the view settles.
    check_ops_on_layout(
        &mut layout,
        [Op::ViewOffsetGestureEnd { is_touchpad: None }],
    );
    let target = format_ipc_positions(&layout);
    assert_ne!(target, before);

    check_ops_on_layout(&mut layout, [Op::CompleteAnimations]);
    assert_eq!(format_ipc_positions(&layout), target);
    assert_eq!(format_render_positions(&layout), target);
}
