//! A press on a workspace label in the overview: released in place, it reports a click to IPC
//! clients (a desktop shell may offer to rename the workspace); dragged, it moves the workspace
//! up or down the stack, following the pointer.

use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, GestureHoldBeginEvent,
    GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
    GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::input::SeatHandler;
use smithay::output::Output;
use smithay::utils::{Logical, Point};

use crate::layout::workspace::WorkspaceId;
use crate::niri::State;

/// How far the pointer moves before a press on a label becomes a drag.
const DRAG_THRESHOLD: f64 = 6.;

pub struct WorkspaceLabelGrab {
    start_data: PointerGrabStartData<State>,
    output: Output,
    workspace_id: WorkspaceId,
    dragging: bool,
}

impl WorkspaceLabelGrab {
    pub fn new(
        start_data: PointerGrabStartData<State>,
        output: Output,
        workspace_id: WorkspaceId,
    ) -> Self {
        Self {
            start_data,
            output,
            workspace_id,
            dragging: false,
        }
    }

    fn on_motion(&mut self, data: &mut State, location: Point<f64, Logical>) {
        let Some(output_geo) = data.niri.global_space.output_geometry(&self.output) else {
            return;
        };
        let y = location.y - f64::from(output_geo.loc.y);
        if !self.dragging {
            let delta = location - self.start_data.location;
            if delta.x.hypot(delta.y) < DRAG_THRESHOLD {
                return;
            }
            self.dragging = true;
            data.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::Named(CursorIcon::Grabbing));
            let start_y = self.start_data.location.y - f64::from(output_geo.loc.y);
            if let Some(monitor) = data.niri.layout.monitor_for_output_mut(&self.output) {
                monitor.workspace_drag_begin(self.workspace_id, start_y);
            }
        }

        let layout = &mut data.niri.layout;
        let Some(monitor) = layout.monitor_for_output_mut(&self.output) else {
            return;
        };
        let target = monitor.workspace_drag_update(y);
        let current = monitor.workspace_idx(self.workspace_id);
        if let (Some(target), Some(current)) = (target, current) {
            if target != current {
                // The others slide aside as the workspace passes them.
                let before = monitor.workspace_order();
                layout.move_workspace_to_idx(Some((Some(self.output.clone()), current)), target);
                if let Some(monitor) = layout.monitor_for_output_mut(&self.output) {
                    monitor.animate_workspace_reorder(&before);
                }
            }
        }
        // FIXME: granular.
        data.niri.queue_redraw_all();
    }

    fn finish(&mut self, data: &mut State) {
        if !self.dragging {
            return;
        }
        if let Some(monitor) = data.niri.layout.monitor_for_output_mut(&self.output) {
            monitor.workspace_drag_end();
        }
        data.niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        data.niri.queue_redraw_all();
    }
}

impl PointerGrab<State> for WorkspaceLabelGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // No client sees the pointer while it holds a label.
        handle.motion(data, None, event);
        self.on_motion(data, event.location);
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if !handle.current_pressed().is_empty() {
            return;
        }
        let clicked = !self.dragging;
        let id = self.workspace_id.get();
        handle.unset_grab(self, data, event.serial, event.time, true);
        if clicked {
            data.ipc_overview_workspace_label_clicked(id);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut State) {
        // Released, or the grab was taken away: the workspace settles in its slot.
        self.finish(data);
        self.dragging = false;
    }
}
