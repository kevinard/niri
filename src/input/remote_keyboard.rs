//! Input events for compositor-owned keyboard leases.
//! Backend associated types derive from Smithay virtual_keyboard/input.rs at
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

use smithay::backend::input::{self, InputBackend, InputTime, UnusedEvent};
use smithay::input::keyboard::KeyboardSource;
use smithay::output::Output;

use super::backend_ext::NiriInputDevice;
use crate::niri::State;

#[derive(Debug)]
pub struct RemoteKeyboardBackend;

impl InputBackend for RemoteKeyboardBackend {
    type Device = RemoteKeyboardDevice;
    type KeyboardKeyEvent = RemoteKeyboardKeyEvent;

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
pub struct RemoteKeyboardDevice {
    pub source: KeyboardSource,
}

impl input::Device for RemoteKeyboardDevice {
    fn id(&self) -> String {
        format!("remote-keyboard-{:?}", self.source)
    }
    fn name(&self) -> String {
        "remote keyboard".into()
    }
    fn has_capability(&self, cap: input::DeviceCapability) -> bool {
        cap == input::DeviceCapability::Keyboard
    }
    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }
    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

impl NiriInputDevice for RemoteKeyboardDevice {
    fn output(&self, _: &State) -> Option<Output> {
        None
    }
}

#[derive(Debug)]
pub struct RemoteKeyboardKeyEvent {
    pub device: RemoteKeyboardDevice,
    pub key: u32,
    pub state: input::KeyState,
}

impl input::Event<RemoteKeyboardBackend> for RemoteKeyboardKeyEvent {
    fn time(&self) -> InputTime {
        InputTime::now()
    }
    fn device(&self) -> RemoteKeyboardDevice {
        self.device.clone()
    }
}
impl input::KeyboardKeyEvent<RemoteKeyboardBackend> for RemoteKeyboardKeyEvent {
    fn key_code(&self) -> input::Keycode {
        (self.key + 8).into()
    }
    fn state(&self) -> input::KeyState {
        self.state
    }
    fn count(&self) -> u32 {
        1
    }
}
