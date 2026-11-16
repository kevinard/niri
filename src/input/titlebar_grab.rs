use smithay::backend::input::ButtonState;
use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
    MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
};
use smithay::input::touch::{
    self, GrabStartData as TouchGrabStartData, TouchGrab, TouchInnerHandle,
};
use smithay::input::SeatHandler;
use smithay::utils::{Logical, Point};

use crate::layout::titlebar::TitlebarPart;
use crate::layout::HitType;
use crate::niri::State;

pub struct TitlebarButtonGrab {
    start_data: PointerGrabStartData<State>,
    window: Window,
    part: TitlebarPart,
}

pub struct TitlebarTouchGrab {
    start_data: TouchGrabStartData<State>,
    window: Window,
    part: TitlebarPart,
    location: Point<f64, Logical>,
}

impl TitlebarTouchGrab {
    pub fn new(start_data: TouchGrabStartData<State>, window: Window, part: TitlebarPart) -> Self {
        let location = start_data.location;
        Self {
            start_data,
            window,
            part,
            location,
        }
    }
}

impl TouchGrab<State> for TitlebarTouchGrab {
    fn down(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _: Option<(<State as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &touch::DownEvent,
    ) {
        handle.down(data, None, event);
    }

    fn up(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::UpEvent,
    ) {
        handle.up(data, event);
        if event.slot != self.start_data.slot {
            return;
        }
        let contents = data.niri.contents_under(self.location);
        if !data.niri.is_locked()
            && contents.window.as_ref().is_some_and(|(window, hit)| {
                window == &self.window
                    && matches!(hit, HitType::Titlebar { part } if *part == self.part)
            })
        {
            data.perform_titlebar_action(&self.window, self.part);
        }
        handle.unset_grab(self, data);
    }

    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        _: Option<(<State as SeatHandler>::TouchFocus, Point<f64, Logical>)>,
        event: &touch::MotionEvent,
    ) {
        handle.motion(data, None, event);
        if event.slot == self.start_data.slot {
            self.location = event.location;
        }
    }

    fn frame(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>) {
        handle.frame(data);
    }
    fn cancel(&mut self, data: &mut State, handle: &mut TouchInnerHandle<'_, State>) {
        handle.cancel(data);
        handle.unset_grab(self, data);
    }
    fn shape(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::ShapeEvent,
    ) {
        handle.shape(data, event);
    }
    fn orientation(
        &mut self,
        data: &mut State,
        handle: &mut TouchInnerHandle<'_, State>,
        event: &touch::OrientationEvent,
    ) {
        handle.orientation(data, event);
    }
    fn start_data(&self) -> &TouchGrabStartData<State> {
        &self.start_data
    }
    fn unset(&mut self, _: &mut State) {}
}

impl TitlebarButtonGrab {
    pub fn new(
        start_data: PointerGrabStartData<State>,
        window: Window,
        part: TitlebarPart,
    ) -> Self {
        Self {
            start_data,
            window,
            part,
        }
    }

    fn on_ungrab(&mut self, state: &mut State) {
        state
            .niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        // Redraw to update the cursor.
        state.niri.queue_redraw_all();
    }
}

impl PointerGrab<State> for TitlebarButtonGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
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
        // A button activates only on release over the same control of the same live window.
        // Every other pointer button is swallowed for the duration of this compositor grab.
        if event.button != self.start_data.button {
            if event.state == ButtonState::Pressed {
                data.niri.suppressed_buttons.insert(event.button);
            }
            return;
        }
        if event.state != ButtonState::Released {
            return;
        }
        let location = handle.current_location();
        let contents = data.niri.contents_under(location);
        if !data.niri.is_locked()
            && contents.window.as_ref().is_some_and(|(window, hit)| {
                window == &self.window
                    && matches!(hit, HitType::Titlebar { part, .. } if *part == self.part)
            })
        {
            data.perform_titlebar_action(&self.window, self.part);
        }

        handle.unset_grab(self, data, event.serial, event.time, true);
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
        self.on_ungrab(data);
    }
}
