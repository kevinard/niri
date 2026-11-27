### Overview

<sup>Since: 25.01</sup>

Floating windows in niri always show on top of the tiled windows.
The floating layout does not scroll.
Each workspace/monitor has its own floating layout, just like each workspace/monitor has its own tiling layout.

New windows will automatically float if they have a parent (e.g. dialogs) or if they are fixed size (e.g. splash screens).
To change a window between floating and tiling, you can use the `toggle-window-floating` bind or right click while dragging/moving the window.
You can also use the `open-floating true/false` window rule to either force a window to open as floating, or to disable the automatic floating logic.

Use `switch-focus-between-floating-and-tiling` to switch the focus between the two layouts.
When focused on the floating layout, binds (like `focus-column-right`) will operate on the floating window.

You can precisely position a floating window with a command like `niri msg action move-floating-window -x 100 -y 200`.

Dragged or resized with the pointer, a floating window snaps when an edge comes within 14 logical pixels of a line: the edges of the working area one gap in, the edges of the other shown floating windows (aligned, or one gap apart), and, while moving, the middle of the working area for the window's middle.

### Scratchpad

The scratchpad is a set of floating windows laid over whatever workspace you are on, like a special workspace: they come and go together, each where you last put it.

```kdl
binds {
    Mod+Grave { toggle-scratchpad; }
    Mod+Shift+Grave { toggle-window-in-scratchpad; }
}
```

`toggle-window-in-scratchpad` adds the focused window (or the one given with `--id` over IPC) to the scratchpad and hides it.
A tiled window becomes floating first, at 60% of the working area's width and 70% of its height, centered.
Used on a window already in the scratchpad, it takes the window out, leaving it an ordinary floating window.

`toggle-scratchpad` brings every scratchpad window to the focused workspace, each at the position and size it had, and focuses the topmost.
Pressed while one of them has focus, it hides them all; if they are shown but another window has focus, the topmost scratchpad window takes it.
Arrange them like any floating windows: moved or resized, they come back that way, on any workspace or monitor.
While a scratchpad window has focus, everything behind the scratchpad dims softly; focusing another window lifts the dimming.
A hidden window is neither drawn nor under the pointer, and focus never stays on it.
It still belongs to its workspace and stays in `niri msg windows`, where its layout reports `in_scratchpad`; focusing it, for example from a taskbar, shows it where it is.
While the scratchpad holds windows, the window bar of `clip-scroll-to-work-area` shows a button for it, which toggles the scratchpad.
The [`open-in-scratchpad`](./Configuration:-Window-Rules.md#open-in-scratchpad) window rule sends a window to the scratchpad when it opens.
