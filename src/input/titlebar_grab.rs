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

use super::DOUBLE_CLICK_TIME;
use crate::layout::titlebar::{self, TitlebarPart};
use crate::layout::{HitType, LayoutElement as _};
use crate::niri::State;
use crate::utils::get_monotonic_time;

impl State {
    /// Closes or maximizes `window` if it is still mapped.
    pub(super) fn perform_titlebar_action(&mut self, window: &Window, part: TitlebarPart) {
        let Some(fixed_size) = self
            .niri
            .layout
            .windows()
            .find(|(_, mapped)| &mapped.window == window)
            .map(|(_, mapped)| mapped.is_fixed_size())
        else {
            return;
        };
        match part {
            TitlebarPart::Close => {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
            // The button is hidden for fixed-size windows; a double click must not maximize
            // them either.
            TitlebarPart::Maximize if fixed_size => return,
            TitlebarPart::Maximize => self.niri.layout.toggle_maximized(window),
            TitlebarPart::Drag => return,
        }
        self.niri.queue_redraw_all();
    }

    /// Records a press on the drag area of `window`'s titlebar. Returns `true` and maximizes the
    /// window when the press completes a double click or tap.
    pub(super) fn titlebar_drag_press(
        &mut self,
        window: &Window,
        location: Point<f64, Logical>,
    ) -> bool {
        // Farther apart, the presses are separate drags rather than a double click.
        const MAX_DISTANCE: f64 = 16.;
        let now = get_monotonic_time();
        let double =
            self.niri
                .titlebar_last_press
                .take()
                .is_some_and(|(last, time, last_location)| {
                    let delta = location - last_location;
                    &last == window
                        && now.saturating_sub(time) <= DOUBLE_CLICK_TIME
                        && delta.x.hypot(delta.y) <= MAX_DISTANCE
                });
        let action = self.niri.config.borrow().layout.titlebar.double_click;
        if double && action == niri_config::titlebar::DoubleClickAction::Maximize {
            self.perform_titlebar_action(window, TitlebarPart::Maximize);
            return true;
        }
        self.niri.titlebar_last_press = Some((window.clone(), now, location));
        false
    }

    // The press state is applied by refresh_titlebar_interaction() before the next redraw. It
    // cannot run here: grab callbacks hold the seat's pointer lock.
    pub(super) fn begin_titlebar_press(&mut self, window: &Window, part: TitlebarPart) {
        self.niri.titlebar_press = Some((window.clone(), part, true));
        self.niri.queue_redraw_all();
    }

    fn update_titlebar_press(&mut self, location: Point<f64, Logical>) {
        let Some((window, part, was_over)) = self.niri.titlebar_press.clone() else {
            return;
        };
        let over = self.is_over_titlebar_part(location, &window, part);
        if over != was_over {
            self.niri.titlebar_press = Some((window, part, over));
            self.niri.queue_redraw_all();
        }
    }

    fn end_titlebar_press(&mut self) {
        if self.niri.titlebar_press.take().is_some() {
            self.niri.queue_redraw_all();
        }
    }

    fn is_over_titlebar_part(
        &self,
        location: Point<f64, Logical>,
        window: &Window,
        part: TitlebarPart,
    ) -> bool {
        !self.niri.is_locked()
            && self
                .niri
                .contents_under(location)
                .window
                .is_some_and(|(under, hit)| {
                    &under == window && matches!(hit, HitType::Titlebar { part: p } if p == part)
                })
    }

    /// Updates the hovered and pressed titlebar controls and the cursor over titlebars.
    pub(crate) fn refresh_titlebar_interaction(&mut self) {
        let pointer = self.niri.seat.get_pointer().unwrap();
        let free_pointer = !pointer.is_grabbed()
            && self.niri.pointer_visibility.is_visible()
            && !self.niri.is_locked()
            && !self.niri.screenshot_ui.is_open()
            && self.niri.dnd_icon.is_none();
        let under_pointer = match &self.niri.pointer_contents.window {
            Some((window, HitType::Titlebar { part })) => Some((window.clone(), *part)),
            _ => None,
        };

        let target = match &self.niri.titlebar_press {
            Some((window, part, true)) => {
                Some((window.clone(), (*part, titlebar::ButtonState::Pressed)))
            }
            Some(_) => None,
            None => under_pointer
                .clone()
                .filter(|(_, part)| free_pointer && *part != TitlebarPart::Drag)
                .map(|(window, part)| (window, (part, titlebar::ButtonState::Hovered))),
        };

        // Clients set their cursor on pointer enter; a titlebar has no client to do that, so the
        // last client cursor (a text beam, a resize arrow, or none) would otherwise remain.
        let over_titlebar = under_pointer.is_some();
        if over_titlebar && !self.niri.pointer_over_titlebar && free_pointer {
            self.niri
                .cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
            self.niri.queue_redraw_all();
        }
        self.niri.pointer_over_titlebar = over_titlebar;

        let mut changed = false;
        let previous = self.niri.titlebar_interaction_window.take();
        if let Some(previous) = previous {
            if target
                .as_ref()
                .is_none_or(|(window, _)| *window != previous)
            {
                if let Some((_, mapped)) = self
                    .niri
                    .layout
                    .windows()
                    .find(|(_, mapped)| mapped.window == previous)
                {
                    changed |= mapped.set_titlebar_interaction(None);
                }
            }
        }
        if let Some((window, interaction)) = target {
            if let Some((_, mapped)) = self
                .niri
                .layout
                .windows()
                .find(|(_, mapped)| mapped.window == window)
            {
                changed |= mapped.set_titlebar_interaction(Some(interaction));
                self.niri.titlebar_interaction_window = Some(window);
            }
        }
        if changed {
            self.niri.queue_redraw_all();
        }
    }
}

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
        if data.is_over_titlebar_part(self.location, &self.window, self.part) {
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
            data.update_titlebar_press(event.location);
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
    fn unset(&mut self, data: &mut State) {
        data.end_titlebar_press();
    }
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
        data.update_titlebar_press(event.location);
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
        // Other buttons pass through the grab without a focus, so no client receives them and
        // the seat still sees their releases.
        handle.button(data, event);
        if event.button != self.start_data.button || event.state != ButtonState::Released {
            return;
        }

        // A button activates only on release over the same control of the same live window.
        let location = handle.current_location();
        if data.is_over_titlebar_part(location, &self.window, self.part) {
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
        data.end_titlebar_press();
    }
}
