//! Pointer events for compositor-owned leases, routed through ordinary Niri input.
//! Backend associated types follow Smithay virtual_pointer/input.rs at
//! 8a270232f1bfc416c781273c98a3f52a812a0bba.
/*
MIT License

Copyright (c) 2017 Victor Berger and Victoria Brekenfeld

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
*/

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use smithay::backend::input::{self, InputBackend, InputTime, UnusedEvent};
use smithay::output::Output;

use super::backend_ext::NiriInputDevice;
use crate::niri::State;

#[derive(Debug)]
pub struct RemotePointerBackend;
impl InputBackend for RemotePointerBackend {
    type Device = RemotePointerDevice;
    type KeyboardKeyEvent = UnusedEvent;
    type PointerAxisEvent = RemotePointerAxisEvent;
    type PointerButtonEvent = RemotePointerButtonEvent;
    type PointerMotionEvent = RemotePointerMotionEvent;
    type PointerMotionAbsoluteEvent = RemotePointerAbsoluteEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePointerDevice(u64);
impl RemotePointerDevice {
    pub fn new() -> Option<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()
            .map(Self)
    }
}
impl input::Device for RemotePointerDevice {
    fn id(&self) -> String {
        format!("remote-pointer-{}", self.0)
    }
    fn name(&self) -> String {
        "remote pointer".into()
    }
    fn has_capability(&self, cap: input::DeviceCapability) -> bool {
        cap == input::DeviceCapability::Pointer
    }
    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }
    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}
impl NiriInputDevice for RemotePointerDevice {
    fn absolute_position_is_global(&self) -> bool {
        true
    }

    fn output(&self, _: &State) -> Option<Output> {
        None
    }
}

#[derive(Debug)]
pub struct RemotePointerButtonEvent {
    pub device: RemotePointerDevice,
    pub time: InputTime,
    pub code: u32,
    pub state: input::ButtonState,
}
impl input::Event<RemotePointerBackend> for RemotePointerButtonEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> RemotePointerDevice {
        self.device.clone()
    }
}
impl input::PointerButtonEvent<RemotePointerBackend> for RemotePointerButtonEvent {
    fn button_code(&self) -> u32 {
        self.code
    }
    fn state(&self) -> input::ButtonState {
        self.state
    }
}

#[derive(Debug)]
pub struct RemotePointerMotionEvent {
    pub device: RemotePointerDevice,
    pub time: InputTime,
    pub x: f64,
    pub y: f64,
}
impl input::Event<RemotePointerBackend> for RemotePointerMotionEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> RemotePointerDevice {
        self.device.clone()
    }
}
impl input::PointerMotionEvent<RemotePointerBackend> for RemotePointerMotionEvent {
    fn delta_x(&self) -> f64 {
        self.x
    }
    fn delta_y(&self) -> f64 {
        self.y
    }
    fn delta_x_unaccel(&self) -> f64 {
        self.x
    }
    fn delta_y_unaccel(&self) -> f64 {
        self.y
    }
}

/// Constructed only after a live capture binding resolved stream coordinates.
#[derive(Debug)]
pub struct RemotePointerAbsoluteEvent {
    pub device: RemotePointerDevice,
    pub time: InputTime,
    pub position: smithay::utils::Point<f64, smithay::utils::Logical>,
}
impl input::Event<RemotePointerBackend> for RemotePointerAbsoluteEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> RemotePointerDevice {
        self.device.clone()
    }
}
impl input::AbsolutePositionEvent<RemotePointerBackend> for RemotePointerAbsoluteEvent {
    fn x(&self) -> f64 {
        self.position.x
    }
    fn y(&self) -> f64 {
        self.position.y
    }
    fn x_transformed(&self, _: i32) -> f64 {
        self.position.x
    }
    fn y_transformed(&self, _: i32) -> f64 {
        self.position.y
    }
}
impl input::PointerMotionAbsoluteEvent<RemotePointerBackend> for RemotePointerAbsoluteEvent {}

#[derive(Debug)]
pub struct RemotePointerAxisEvent {
    pub device: RemotePointerDevice,
    pub time: InputTime,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub source: input::AxisSource,
    pub discrete: bool,
}
impl input::Event<RemotePointerBackend> for RemotePointerAxisEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> RemotePointerDevice {
        self.device.clone()
    }
}
impl RemotePointerAxisEvent {
    fn value(&self, axis: input::Axis) -> Option<f64> {
        match axis {
            input::Axis::Horizontal => self.x,
            input::Axis::Vertical => self.y,
        }
    }
}
impl input::PointerAxisEvent<RemotePointerBackend> for RemotePointerAxisEvent {
    fn amount(&self, axis: input::Axis) -> Option<f64> {
        if self.discrete {
            None
        } else {
            self.value(axis)
        }
    }
    fn amount_v120(&self, axis: input::Axis) -> Option<f64> {
        if self.discrete {
            self.value(axis)
        } else {
            None
        }
    }
    fn source(&self) -> input::AxisSource {
        self.source
    }
    fn relative_direction(&self, _: input::Axis) -> input::AxisRelativeDirection {
        input::AxisRelativeDirection::Identical
    }
}
