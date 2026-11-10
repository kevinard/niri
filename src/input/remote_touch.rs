//! Remote touch uses compositor-owned seat slots, never client-provided slots.
pub mod contacts;

use super::backend_ext::NiriInputDevice;
use crate::niri::State;
use smithay::backend::input::{self, InputBackend, InputTime, TouchSlot, UnusedEvent};
use smithay::output::Output;
use smithay::utils::{Logical, Point};
use std::any::Any;
use std::path::PathBuf;

#[derive(Debug)]
pub struct Backend;
impl InputBackend for Backend {
    type Device = Device;
    type KeyboardKeyEvent = UnusedEvent;
    type PointerAxisEvent = UnusedEvent;
    type PointerButtonEvent = UnusedEvent;
    type PointerMotionEvent = UnusedEvent;
    type PointerMotionAbsoluteEvent = UnusedEvent;
    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;
    type TouchDownEvent = PositionEvent;
    type TouchMotionEvent = PositionEvent;
    type TouchUpEvent = UpEvent;
    type TouchFrameEvent = FrameEvent;
    type TouchCancelEvent = UnusedEvent;
    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;
    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Device(pub contacts::Source);
pub fn is_device(device: &dyn Any) -> bool {
    device.is::<Device>()
}
impl input::Device for Device {
    fn id(&self) -> String {
        format!("remote-touch-{:?}", self.0)
    }
    fn name(&self) -> String {
        "remote touch".into()
    }
    fn has_capability(&self, capability: input::DeviceCapability) -> bool {
        capability == input::DeviceCapability::Touch
    }
    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }
    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}
impl NiriInputDevice for Device {
    fn absolute_position_is_global(&self) -> bool {
        true
    }
    fn output(&self, _: &State) -> Option<Output> {
        None
    }
}

#[derive(Debug)]
pub struct PositionEvent {
    pub device: Device,
    pub time: InputTime,
    pub slot: TouchSlot,
    pub position: Point<f64, Logical>,
}
impl input::Event<Backend> for PositionEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> Device {
        self.device.clone()
    }
}
impl input::TouchEvent<Backend> for PositionEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}
impl input::AbsolutePositionEvent<Backend> for PositionEvent {
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
impl input::TouchDownEvent<Backend> for PositionEvent {}
impl input::TouchMotionEvent<Backend> for PositionEvent {}

#[derive(Debug)]
pub struct UpEvent {
    pub device: Device,
    pub time: InputTime,
    pub slot: TouchSlot,
}
impl input::Event<Backend> for UpEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> Device {
        self.device.clone()
    }
}
impl input::TouchEvent<Backend> for UpEvent {
    fn slot(&self) -> TouchSlot {
        self.slot
    }
}
impl input::TouchUpEvent<Backend> for UpEvent {}

#[derive(Debug)]
pub struct FrameEvent {
    pub device: Device,
    pub time: InputTime,
}
impl input::Event<Backend> for FrameEvent {
    fn time(&self) -> InputTime {
        self.time
    }
    fn device(&self) -> Device {
        self.device.clone()
    }
}
impl input::TouchFrameEvent<Backend> for FrameEvent {}
