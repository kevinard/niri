use super::*;
use crate::layout::scrolling::ScrollingSpace;

/// A window that opens with a fixed width.
fn window(id: usize, width: i32) -> TestWindowParams {
    TestWindowParams {
        bbox: Rectangle::from_size(Size::from((width, 200))),
        ..TestWindowParams::new(id)
    }
}

/// Lets every window take its requested size and finishes all animations.
fn settle() -> Vec<Op> {
    let mut ops: Vec<Op> = (1..=5).map(Op::Communicate).collect();
    ops.push(Op::Refresh { is_active: true });
    ops.push(Op::CompleteAnimations);
    ops
}

/// Four 500-wide columns on a 1280-wide output, with the first one pinned to the left.
fn four_columns_first_pinned_left() -> Layout<TestWindow> {
    let mut ops = vec![Op::AddOutput(1)];
    for id in 1..=4 {
        ops.push(Op::AddWindow {
            params: window(id, 500),
        });
    }
    ops.push(Op::FocusColumnFirst);
    ops.push(Op::PinColumn {
        id: None,
        side: PinSide::Left,
    });
    ops.extend(settle());
    check_ops(ops)
}

fn scrolling(layout: &Layout<TestWindow>) -> &ScrollingSpace<TestWindow> {
    layout.active_workspace().unwrap().scrolling()
}

fn tile_x(layout: &Layout<TestWindow>, id: usize) -> f64 {
    layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .find(|(tile, _, _)| *tile.window().id() == id)
        .unwrap()
        .1
        .x
}

fn ipc_layout(layout: &Layout<TestWindow>, id: usize) -> WindowLayout {
    layout
        .workspaces()
        .flat_map(|(_, _, ws)| ws.tiles_with_ipc_layouts())
        .find(|(tile, _)| *tile.window().id() == id)
        .unwrap()
        .1
}

fn column_of(layout: &Layout<TestWindow>, id: usize) -> usize {
    ipc_layout(layout, id).pos_in_scrolling_layout.unwrap().0
}

fn tile_width(layout: &Layout<TestWindow>, id: usize) -> f64 {
    ipc_layout(layout, id).tile_size.0
}

fn focused(layout: &Layout<TestWindow>) -> usize {
    *layout.focus().unwrap().id()
}

fn pinned_left_x(layout: &Layout<TestWindow>) -> f64 {
    let gaps = layout.options.layout.gaps;
    scrolling(layout).full_working_area().loc.x + gaps
}

#[test]
fn pinned_column_stays_at_edge_while_focus_scrolls() {
    let mut layout = four_columns_first_pinned_left();
    let gaps = layout.options.layout.gaps;
    let pinned_x = pinned_left_x(&layout);

    assert_eq!(focused(&layout), 1);
    assert_eq!(tile_x(&layout, 1), pinned_x);

    for expected in 2..=4 {
        check_ops_on_layout(&mut layout, [Op::FocusColumnRight, Op::CompleteAnimations]);
        assert_eq!(focused(&layout), expected);
        assert_eq!(
            tile_x(&layout, 1),
            pinned_x,
            "the pinned column must not scroll"
        );

        // The focused column is fully within the area left for scrolling.
        let area = scrolling(&layout).working_area();
        let x = tile_x(&layout, expected);
        assert!(
            area.loc.x + gaps <= x,
            "{x} must be right of the pinned column"
        );
        assert!(x + tile_width(&layout, expected) <= area.loc.x + area.size.w);
    }

    // The first scrolling column went off screen, under and past the pinned column.
    let area = scrolling(&layout).working_area();
    assert!(tile_x(&layout, 2) < area.loc.x);

    // The pinned column draws above the scrolling columns that scroll underneath it.
    let order: Vec<usize> = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .map(|(tile, _, _)| *tile.window().id())
        .collect();
    assert_eq!(order, [1, 4, 2, 3]);
}

#[test]
fn focus_moves_into_and_out_of_pinned_column() {
    let mut layout = four_columns_first_pinned_left();

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight, Op::CompleteAnimations]);
    assert_eq!(focused(&layout), 2);
    let x2 = tile_x(&layout, 2);

    // Going left from the first scrolling column enters the pinned column without scrolling.
    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft, Op::CompleteAnimations]);
    assert_eq!(focused(&layout), 1);
    assert_eq!(tile_x(&layout, 2), x2);

    check_ops_on_layout(&mut layout, [Op::FocusColumnLeft, Op::CompleteAnimations]);
    assert_eq!(
        focused(&layout),
        1,
        "nothing is left of a left-pinned column"
    );

    check_ops_on_layout(&mut layout, [Op::FocusColumnRight, Op::CompleteAnimations]);
    assert_eq!(focused(&layout), 2);

    // Focusing the pinned column from far away keeps the view where it is.
    check_ops_on_layout(&mut layout, [Op::FocusColumnLast, Op::CompleteAnimations]);
    assert_eq!(focused(&layout), 4);
    let x4 = tile_x(&layout, 4);
    check_ops_on_layout(&mut layout, [Op::FocusColumnFirst, Op::CompleteAnimations]);
    assert_eq!(focused(&layout), 1);
    assert_eq!(tile_x(&layout, 4), x4);

    // A column pinned to the right is reached at the other end.
    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumn(3),
            Op::PinColumn {
                id: None,
                side: PinSide::Right,
            },
            Op::FocusColumnFirst,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
            Op::FocusColumnRight,
        ],
    );
    assert_eq!(focused(&layout), 3);
    assert_eq!(column_of(&layout, 3), 4);
    assert_eq!(ipc_layout(&layout, 3).pinned, Some(PinSide::Right));
}

#[test]
fn unpin_restores_order() {
    let mut ops = vec![Op::AddOutput(1)];
    for id in 1..=4 {
        ops.push(Op::AddWindow {
            params: window(id, 300),
        });
    }
    ops.extend([
        Op::FocusColumn(3),
        Op::PinColumn {
            id: None,
            side: PinSide::Left,
        },
    ]);
    let mut layout = check_ops(ops);

    // The pinned column moves to the left end and the others keep their order.
    let columns: Vec<_> = (1..=4).map(|id| column_of(&layout, id)).collect();
    assert_eq!(columns, [2, 3, 1, 4]);
    assert_eq!(ipc_layout(&layout, 3).pinned, Some(PinSide::Left));
    assert_eq!(ipc_layout(&layout, 1).pinned, None);

    // Unpinning returns it to the scrolling columns at the edge it was pinned to.
    check_ops_on_layout(&mut layout, [Op::UnpinColumn { id: None }]);
    let columns: Vec<_> = (1..=4).map(|id| column_of(&layout, id)).collect();
    assert_eq!(columns, [2, 3, 1, 4]);
    assert_eq!(ipc_layout(&layout, 3).pinned, None);
    assert_eq!(
        scrolling(&layout).working_area(),
        scrolling(&layout).full_working_area()
    );

    // The same at the right edge, by window id, and with the toggle.
    check_ops_on_layout(
        &mut layout,
        [
            Op::PinColumn {
                id: Some(1),
                side: PinSide::Right,
            },
            Op::FocusWindow(1),
        ],
    );
    let columns: Vec<_> = (1..=4).map(|id| column_of(&layout, id)).collect();
    assert_eq!(columns, [4, 2, 1, 3]);
    check_ops_on_layout(
        &mut layout,
        [Op::TogglePinnedColumn {
            side: PinSide::Left,
        }],
    );
    let columns: Vec<_> = (1..=4).map(|id| column_of(&layout, id)).collect();
    assert_eq!(columns, [4, 2, 1, 3]);
    assert!((1..=4).all(|id| ipc_layout(&layout, id).pinned.is_none()));
}

#[test]
fn closing_pinned_window_unpins() {
    let mut layout = four_columns_first_pinned_left();
    assert_ne!(
        scrolling(&layout).working_area(),
        scrolling(&layout).full_working_area()
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(1)]);
    check_ops_on_layout(&mut layout, settle());
    assert!((2..=4).all(|id| ipc_layout(&layout, id).pinned.is_none()));
    assert_eq!(
        scrolling(&layout).working_area(),
        scrolling(&layout).full_working_area()
    );

    // Closing one window of a pinned column with several windows keeps the column pinned.
    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusWindow(3),
            Op::ConsumeOrExpelWindowLeft { id: None },
            Op::PinColumn {
                id: None,
                side: PinSide::Right,
            },
            Op::CloseWindow(3),
        ],
    );
    assert_eq!(ipc_layout(&layout, 2).pinned, Some(PinSide::Right));
    assert_eq!(column_of(&layout, 2), 2);
}

#[test]
fn view_width_accounting() {
    let mut ops = vec![
        Op::AddOutput(1),
        Op::AddWindow {
            params: window(1, 400),
        },
        Op::PinColumn {
            id: None,
            side: PinSide::Left,
        },
        Op::AddWindow {
            params: window(2, 300),
        },
        Op::SetColumnWidth(SizeChange::SetProportion(50.)),
    ];
    ops.extend(settle());
    let mut layout = check_ops(ops);

    let gaps = layout.options.layout.gaps;
    let full = scrolling(&layout).full_working_area();
    let area = scrolling(&layout).working_area();
    let w1 = tile_width(&layout, 1);

    // The scrolling area leaves out the pinned column and one gap.
    assert_eq!(area.loc.x, full.loc.x + w1 + gaps);
    assert_eq!(area.size.w, full.size.w - w1 - gaps);

    // Proportional widths are relative to the area left for scrolling.
    assert_eq!(tile_width(&layout, 2), (area.size.w - gaps) * 0.5 - gaps);

    // The new column opens right of the pinned one, at the start of the scrolling area.
    assert_eq!(column_of(&layout, 2), 2);
    assert_eq!(tile_x(&layout, 2), area.loc.x + gaps);

    // A column pinned to the right narrows the area from the right.
    let mut ops = vec![
        Op::AddWindow {
            params: window(3, 200),
        },
        Op::PinColumn {
            id: None,
            side: PinSide::Right,
        },
    ];
    ops.extend(settle());
    check_ops_on_layout(&mut layout, ops);
    let w3 = tile_width(&layout, 3);
    let area = scrolling(&layout).working_area();
    assert_eq!(area.loc.x, full.loc.x + w1 + gaps);
    assert_eq!(area.size.w, full.size.w - w1 - w3 - gaps * 2.);
    assert_eq!(tile_x(&layout, 3), full.loc.x + full.size.w - gaps - w3);
    assert_eq!(tile_width(&layout, 2), (area.size.w - gaps) * 0.5 - gaps);

    // Unpinning gives the space back.
    let mut ops = vec![
        Op::UnpinColumn { id: Some(1) },
        Op::UnpinColumn { id: Some(3) },
    ];
    ops.extend(settle());
    check_ops_on_layout(&mut layout, ops);
    assert_eq!(scrolling(&layout).working_area(), full);
    assert_eq!(tile_width(&layout, 2), (full.size.w - gaps) * 0.5 - gaps);
}

#[test]
fn moving_pinned_columns_is_refused() {
    let mut layout = four_columns_first_pinned_left();
    let order = |layout: &Layout<TestWindow>| -> Vec<usize> {
        (1..=4).map(|id| column_of(layout, id)).collect()
    };
    assert_eq!(order(&layout), [1, 2, 3, 4]);

    // The pinned column does not move within the workspace.
    check_ops_on_layout(
        &mut layout,
        [
            Op::MoveColumnRight,
            Op::MoveColumnToLast,
            Op::MoveColumnToIndex(3),
            Op::SwapWindowInDirection(ScrollDirection::Right),
        ],
    );
    assert_eq!(order(&layout), [1, 2, 3, 4]);

    // Other columns don't move past it.
    check_ops_on_layout(
        &mut layout,
        [
            Op::FocusColumnRight,
            Op::MoveColumnLeft,
            Op::MoveColumnToFirst,
            Op::MoveColumnToIndex(1),
            Op::SwapWindowInDirection(ScrollDirection::Left),
        ],
    );
    assert_eq!(order(&layout), [1, 2, 3, 4]);
    assert_eq!(focused(&layout), 2);

    // Moving among the scrolling columns still works.
    check_ops_on_layout(&mut layout, [Op::MoveColumnToLast]);
    assert_eq!(order(&layout), [1, 4, 2, 3]);
    assert_eq!(ipc_layout(&layout, 1).pinned, Some(PinSide::Left));
}

#[test]
fn column_moved_to_another_workspace_is_unpinned() {
    let mut layout = four_columns_first_pinned_left();
    check_ops_on_layout(&mut layout, [Op::MoveColumnToWorkspaceDown(false)]);

    assert_eq!(ipc_layout(&layout, 1).pinned, None);
    assert_eq!(
        scrolling(&layout).working_area(),
        scrolling(&layout).full_working_area()
    );

    let ws = layout
        .workspaces()
        .map(|(_, _, ws)| ws)
        .find(|ws| ws.has_window(&1))
        .unwrap();
    assert_eq!(
        ws.scrolling().working_area(),
        ws.scrolling().full_working_area()
    );
}

#[test]
fn fullscreen_pinned_column_scrolls_until_unfullscreened() {
    let mut layout = four_columns_first_pinned_left();
    let pinned_x = pinned_left_x(&layout);
    let narrowed = scrolling(&layout).working_area();

    let mut ops = vec![Op::FullscreenWindow(1)];
    ops.extend(settle());
    check_ops_on_layout(&mut layout, ops);

    // While fullscreen, the column keeps its pin but covers the view like any fullscreen column.
    assert_eq!(ipc_layout(&layout, 1).pinned, Some(PinSide::Left));
    assert_eq!(tile_x(&layout, 1), 0.);
    assert_eq!(
        scrolling(&layout).working_area(),
        scrolling(&layout).full_working_area()
    );

    let mut ops = vec![Op::FullscreenWindow(1)];
    ops.extend(settle());
    check_ops_on_layout(&mut layout, ops);

    assert_eq!(tile_x(&layout, 1), pinned_x);
    assert_eq!(scrolling(&layout).working_area(), narrowed);
}

#[test]
fn open_pinned_window_rule() {
    let rules = ResolvedWindowRules {
        open_pinned: Some(PinSide::Right),
        ..ResolvedWindowRules::default()
    };
    let mut ops = vec![
        Op::AddOutput(1),
        Op::AddWindow {
            params: window(1, 300),
        },
        Op::AddWindow {
            params: TestWindowParams {
                rules: Some(rules),
                ..window(2, 300)
            },
        },
        Op::AddWindow {
            params: window(3, 300),
        },
    ];
    ops.extend(settle());
    let layout = check_ops(ops);

    assert_eq!(ipc_layout(&layout, 2).pinned, Some(PinSide::Right));
    assert_eq!(column_of(&layout, 2), 3);
    // Later windows open left of the column pinned to the right.
    assert_eq!(column_of(&layout, 3), 2);

    let gaps = layout.options.layout.gaps;
    let full = scrolling(&layout).full_working_area();
    assert_eq!(
        tile_x(&layout, 2),
        full.loc.x + full.size.w - gaps - tile_width(&layout, 2)
    );
}

#[test]
fn expelling_from_pinned_column_stays_between_pinned_columns() {
    let mut ops = vec![
        Op::AddOutput(1),
        Op::AddWindow {
            params: window(1, 300),
        },
        Op::AddWindow {
            params: window(2, 300),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::PinColumn {
            id: None,
            side: PinSide::Left,
        },
        Op::AddWindow {
            params: window(3, 300),
        },
        Op::AddWindow {
            params: window(4, 300),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::PinColumn {
            id: None,
            side: PinSide::Right,
        },
    ];
    ops.extend(settle());
    let mut layout = check_ops(ops);
    let order = |layout: &Layout<TestWindow>| -> Vec<usize> {
        (1..=4).map(|id| column_of(layout, id)).collect()
    };
    assert_eq!(order(&layout), [1, 1, 2, 2]);
    assert_eq!(ipc_layout(&layout, 3).pinned, Some(PinSide::Right));

    // Expelling left out of the left-pinned column puts the window right of it.
    check_ops_on_layout(&mut layout, [Op::ConsumeOrExpelWindowLeft { id: Some(2) }]);
    assert_eq!(order(&layout), [1, 2, 3, 3]);
    assert_eq!(ipc_layout(&layout, 2).pinned, None);

    // Expelling right out of the right-pinned column puts the window left of it.
    check_ops_on_layout(&mut layout, [Op::ConsumeOrExpelWindowRight { id: Some(4) }]);
    assert_eq!(order(&layout), [1, 2, 4, 3]);
    assert_eq!(ipc_layout(&layout, 3).pinned, Some(PinSide::Right));
}

#[test]
fn pinning_replaces_the_column_pinned_to_that_side() {
    let mut layout = four_columns_first_pinned_left();
    check_ops_on_layout(
        &mut layout,
        [
            Op::PinColumn {
                id: Some(3),
                side: PinSide::Left,
            },
            Op::CompleteAnimations,
        ],
    );

    let columns: Vec<_> = (1..=4).map(|id| column_of(&layout, id)).collect();
    assert_eq!(columns, [2, 3, 1, 4]);
    assert_eq!(ipc_layout(&layout, 3).pinned, Some(PinSide::Left));
    assert_eq!(ipc_layout(&layout, 1).pinned, None);
    assert_eq!(tile_x(&layout, 3), pinned_left_x(&layout));
}

#[test]
fn view_gesture_keeps_pinned_column_focused() {
    let mut layout = four_columns_first_pinned_left();
    let pinned_x = pinned_left_x(&layout);
    let x2 = tile_x(&layout, 2);

    check_ops_on_layout(
        &mut layout,
        [
            Op::ViewOffsetGestureBegin {
                output_idx: 1,
                workspace_idx: None,
                is_touchpad: false,
            },
            Op::ViewOffsetGestureUpdate {
                delta: 800.,
                timestamp: Duration::from_millis(100),
                is_touchpad: false,
            },
            Op::ViewOffsetGestureEnd {
                is_touchpad: Some(false),
            },
            Op::CompleteAnimations,
        ],
    );

    assert_eq!(focused(&layout), 1);
    assert_eq!(tile_x(&layout, 1), pinned_x);
    assert!(
        tile_x(&layout, 2) < x2,
        "the other columns must have scrolled"
    );
}

#[test]
fn dropping_onto_pinned_column_inserts_into_it() {
    let layout = four_columns_first_pinned_left();
    let scrolling = scrolling(&layout);
    let pinned_x = pinned_left_x(&layout);

    let pos = Point::from((pinned_x + 10., 50.));
    assert!(matches!(
        scrolling.insert_position(pos),
        InsertPosition::InColumn(0, _)
    ));

    // Right of the pinned column (past the half gap that still belongs to it), a new column goes
    // at the start of the scrolling columns.
    let gaps = layout.options.layout.gaps;
    let area = scrolling.working_area();
    let pos = Point::from((area.loc.x + gaps, 50.));
    assert!(matches!(
        scrolling.insert_position(pos),
        InsertPosition::NewColumn(1)
    ));
}

#[test]
fn resizing_scrolling_columns_keeps_view_while_right_pinned_column_is_focused() {
    let mut ops = vec![Op::AddOutput(1)];
    for id in 1..=3 {
        ops.push(Op::AddWindow {
            params: TestWindowParams::new(id),
        });
        ops.push(Op::SetColumnWidth(SizeChange::SetProportion(30.)));
    }
    ops.extend([
        Op::AddWindow {
            params: window(4, 300),
        },
        Op::PinColumn {
            id: None,
            side: PinSide::Right,
        },
    ]);
    ops.extend(settle());
    let mut layout = check_ops(ops);
    assert_eq!(focused(&layout), 4);
    let view_pos = scrolling(&layout).view_pos();
    let x1 = tile_x(&layout, 1);

    // Shrinking the scrolling columns must not scroll the view: it stays relative to the start
    // of the strip while a pinned column is focused.
    for id in 1..=3 {
        check_ops_on_layout(
            &mut layout,
            [Op::SetWindowWidth {
                id: Some(id),
                change: SizeChange::SetFixed(100),
            }],
        );
    }
    check_ops_on_layout(&mut layout, settle());
    assert_eq!(scrolling(&layout).view_pos(), view_pos);
    assert_eq!(tile_x(&layout, 1), x1);
    assert_eq!(focused(&layout), 4);

    // Adding a column at the end of the strip doesn't move the others either.
    check_ops_on_layout(
        &mut layout,
        [
            Op::AddWindowNextTo {
                params: window(5, 100),
                next_to_id: 3,
            },
            Op::FocusWindow(4),
        ],
    );
    check_ops_on_layout(&mut layout, settle());
    assert_eq!(column_of(&layout, 5), 4);
    assert_eq!(column_of(&layout, 4), 5);
    assert_eq!(tile_x(&layout, 1), x1);
}

#[test]
fn closing_focused_right_pinned_column_keeps_view() {
    let mut ops = vec![Op::AddOutput(1)];
    for id in 1..=3 {
        ops.push(Op::AddWindow {
            params: window(id, 300),
        });
    }
    ops.extend([
        Op::FocusColumnFirst,
        Op::AddWindow {
            params: window(4, 300),
        },
        Op::PinColumn {
            id: None,
            side: PinSide::Right,
        },
    ]);
    ops.extend(settle());
    let mut layout = check_ops(ops);
    assert_eq!(focused(&layout), 4);
    let xs: Vec<f64> = (1..=3).map(|id| tile_x(&layout, id)).collect();

    // Nothing jumps when the focused pinned column closes; the view then animates to the newly
    // focused column.
    check_ops_on_layout(&mut layout, [Op::CloseWindow(4)]);
    assert_eq!(focused(&layout), 3);
    let after: Vec<f64> = (1..=3).map(|id| tile_x(&layout, id)).collect();
    assert_eq!(after, xs);
}
