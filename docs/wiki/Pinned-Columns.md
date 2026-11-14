### Overview

<sup>Since: next release</sup>

You can pin a column to the left or right edge of its workspace.
A pinned column stays on screen while the other columns scroll past, which is handy for chat, music or reference material.

Use these binds to pin the focused column, or to unpin it:

```kdl
binds {
    Mod+Alt+Left { pin-column "left"; }
    Mod+Alt+Right { pin-column "right"; }
    Mod+Alt+Down { unpin-column; }

    // Or a single bind that pins to the left, or unpins a pinned column.
    Mod+P { toggle-pinned-column "left"; }
}
```

The same actions are available over IPC: `niri msg action pin-column left`, `niri msg action unpin-column` and `niri msg action toggle-pinned-column right`.

Each workspace can have one column pinned to the left and one pinned to the right.
Pinning a column to an edge that already has one unpins the old column, which becomes the first (or last) scrolling column.

### How pinned columns behave

- A column pinned to the left moves to the start of the workspace, and a column pinned to the right moves to the end.
  The other columns keep their order.
  Unpinning a column returns it to the scrolling columns at the edge it was pinned to.
- The other columns get the rest of the working area, as if the pinned column was a [strut](./Configuration:-Layout.md#struts) with an extra gap.
  Proportional widths like `proportion 0.5` are relative to this remaining area, so two half-width columns still fit next to a pinned column.
  The pinned column's own proportional width is relative to the whole working area.
  Pinned columns together never take more than three quarters of the working area; wider pinned columns overlap the scrolling columns.
- Focus moves into a pinned column at the end of the workspace: `focus-column-left` from the first scrolling column focuses the column pinned to the left, and `focus-column-first` and `focus-column-last` focus the pinned columns.
  Focusing a pinned column doesn't scroll the view.
  Scrolling the view with a gesture while a pinned column is focused keeps it focused.
- Pinned columns don't move within the workspace: `move-column-left`, `move-column-right` and the other column movement actions do nothing for a pinned column, and other columns can't move past a pinned column.
  `move-column-left-or-to-monitor-left` (or `-right`) still moves a column pinned to that side to the next monitor.
  `swap-window-left` and `swap-window-right` don't swap windows with a pinned column.
- You can still add windows to a pinned column and remove them from it, for example with `consume-or-expel-window-left`.
  Windows expelled from a pinned column go next to it, among the scrolling columns.
  A pinned column stays pinned while it has windows; closing its last window removes it.
- Moving a pinned column (or its last window) to another workspace or monitor, or making its last window floating, unpins it.
  Pinned columns stay pinned when their workspace moves to another monitor.
- A fullscreen or maximized pinned column keeps its pin, but while it is fullscreen or maximized it behaves like an ordinary column at its end of the workspace: the other columns get the whole working area and can scroll it out of view.
  Once it returns to normal, it goes back to its edge.
- `center-column`, `center-visible-columns` and `expand-column-to-available-width` do nothing for a focused pinned column.

### Window rule

The [`open-pinned`](./Configuration:-Window-Rules.md#open-pinned) window rule pins a window's column when it opens.

### IPC

Windows in a pinned column report the edge in the `pinned` field of their layout (`"Left"` or `"Right"`, or `null`).
The column index in `pos_in_scrolling_layout` stays meaningful: a column pinned to the left is always column 1, and a column pinned to the right is always the last column, consistent with `focus-column`.
