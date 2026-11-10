//! FD-owned relative pointer, button and scroll transport. Absolute motion needs
//! a separate live capture-source mapping; this socket never falls back to the
//! whole desktop's bounding rectangle. The portal still owns authorization/UI.

use std::cell::RefCell;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use smithay::backend::input::{AxisSource, ButtonState, InputEvent, InputTime};
use zbus::fdo;

use super::Permit;
use crate::input::remote_pointer::{
    RemotePointerAbsoluteEvent, RemotePointerAxisEvent, RemotePointerBackend,
    RemotePointerButtonEvent, RemotePointerDevice, RemotePointerMotionEvent,
};
use crate::niri::State;
use crate::screencasting::sources::mapping::Bindings;

mod wire;
use wire::{Buttons, Command, Reader, COMMANDS_PER_DISPATCH};

/// Relative motion, buttons, continuous/finger scrolling, discrete v120 scrolling.
/// No absolute or touch bit is included.
pub const CAPABILITIES: u32 = 1 | 2 | 4 | 8;

#[derive(Default, Clone)]
pub struct Leases(Rc<RefCell<Vec<Weak<RefCell<Lease>>>>>);

pub fn refresh(state: &mut State, leases: &Leases) {
    let active: Vec<_> = {
        let mut entries = leases.0.borrow_mut();
        entries.retain(|entry| entry.strong_count() != 0);
        entries.iter().filter_map(Weak::upgrade).collect()
    };
    for lease in active {
        let mut lease = lease.borrow_mut();
        if state.niri.is_locked()
            || lease
                .bindings
                .as_ref()
                .is_some_and(|b| !b.valid(&state.niri))
        {
            lease.release(state);
        }
    }
}

pub fn open(
    state: &mut State,
    stream: UnixStream,
    permit: Permit,
    reply: async_channel::Sender<fdo::Result<u32>>,
    leases: &Leases,
) {
    open_inner(state, stream, permit, reply, leases, None);
}

pub fn open_mapped(
    state: &mut State,
    stream: UnixStream,
    permit: Permit,
    reply: async_channel::Sender<fdo::Result<(u32, super::MappingDescriptors)>>,
    leases: &Leases,
    session: &str,
    streams: &[String],
) {
    let bindings = match Bindings::new(&state.niri, session, streams) {
        Ok(bindings) => bindings,
        Err(error) => {
            let _ = reply.try_send(Err(error));
            return;
        }
    };
    let descriptors = bindings.descriptors();
    let (inner_reply, receive) = async_channel::bounded(1);
    open_inner(state, stream, permit, inner_reply, leases, Some(bindings));
    let result = receive
        .try_recv()
        .unwrap_or_else(|_| Err(fdo::Error::Failed("pointer setup did not finish".into())));
    let _ = reply.try_send(result.map(|caps| (caps, descriptors)));
}

fn open_inner(
    state: &mut State,
    stream: UnixStream,
    permit: Permit,
    reply: async_channel::Sender<fdo::Result<u32>>,
    leases: &Leases,
    bindings: Option<Bindings>,
) {
    if state.niri.is_locked() {
        let _ = reply.try_send(Err(fdo::Error::AccessDenied("session is locked".into())));
        return;
    }
    let Some(device) = RemotePointerDevice::new() else {
        let _ = reply.try_send(Err(fdo::Error::LimitsExceeded(
            "pointer source identities exhausted".into(),
        )));
        return;
    };
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = reply.try_send(Err(fdo::Error::Failed(error.to_string())));
            return;
        }
    };
    let lease = Rc::new(RefCell::new(Lease {
        _permit: permit,
        device,
        reader: Reader::default(),
        buttons: Buttons::default(),
        writer,
        retired: false,
        bindings,
    }));
    let capabilities = CAPABILITIES
        | if lease.borrow().bindings.is_some() {
            16
        } else {
            0
        };
    leases.0.borrow_mut().push(Rc::downgrade(&lease));
    let result = state.niri.event_loop.insert_source(
        Generic::new(stream, Interest::READ, Mode::Level),
        move |_, socket, state| {
            let mut lease = lease.borrow_mut();
            if lease.retired {
                return Ok(PostAction::Remove);
            }
            if state.niri.is_locked()
                || lease
                    .bindings
                    .as_ref()
                    .is_some_and(|b| !b.valid(&state.niri))
            {
                lease.release(state);
                return Ok(PostAction::Remove);
            }
            let mut socket = &**socket;
            for _ in 0..COMMANDS_PER_DISPATCH {
                let (sequence, command) = match lease.reader.next(&mut socket) {
                    Ok(Some(command)) => command,
                    Ok(None) => return Ok(PostAction::Continue),
                    Err(_) => {
                        lease.release(state);
                        return Ok(PostAction::Remove);
                    }
                };
                if lease.dispatch(state, command).is_err()
                    || lease.writer.write(&wire::feedback(sequence)).ok() != Some(16)
                {
                    lease.release(state);
                    return Ok(PostAction::Remove);
                }
            }
            Ok(PostAction::Continue)
        },
    );
    let _ = reply.try_send(
        result
            .map(|_| capabilities)
            .map_err(|e| fdo::Error::Failed(e.to_string())),
    );
}

struct Lease {
    _permit: Permit,
    device: RemotePointerDevice,
    reader: Reader,
    buttons: Buttons,
    writer: UnixStream,
    retired: bool,
    bindings: Option<Bindings>,
}
impl Lease {
    fn dispatch(&mut self, state: &mut State, command: Command) -> std::io::Result<()> {
        let device = self.device.clone();
        let time = InputTime::now();
        match command {
            Command::Barrier => {}
            Command::Absolute { x, y, stream } => {
                state.niri.advance_animations();
                let position = self
                    .bindings
                    .as_ref()
                    .and_then(|b| b.point(&state.niri, stream, x, y))
                    .ok_or(std::io::ErrorKind::InvalidData)?;
                state.process_input_event::<RemotePointerBackend>(
                    InputEvent::PointerMotionAbsolute {
                        event: RemotePointerAbsoluteEvent {
                            device,
                            time,
                            position,
                        },
                    },
                );
            }
            Command::Relative { x, y } => {
                state.process_input_event::<RemotePointerBackend>(InputEvent::PointerMotion {
                    event: RemotePointerMotionEvent { device, time, x, y },
                })
            }
            Command::Button { code, pressed } => {
                self.buttons.transition(code, pressed)?;
                state.process_input_event::<RemotePointerBackend>(InputEvent::PointerButton {
                    event: RemotePointerButtonEvent {
                        device,
                        time,
                        code,
                        state: if pressed {
                            ButtonState::Pressed
                        } else {
                            ButtonState::Released
                        },
                    },
                });
            }
            Command::Scroll { x, y, finger } => {
                state.process_input_event::<RemotePointerBackend>(InputEvent::PointerAxis {
                    event: RemotePointerAxisEvent {
                        device,
                        time,
                        x,
                        y,
                        source: if finger {
                            AxisSource::Finger
                        } else {
                            AxisSource::Continuous
                        },
                        discrete: false,
                    },
                })
            }
            Command::Wheel { x, y } => {
                state.process_input_event::<RemotePointerBackend>(InputEvent::PointerAxis {
                    event: RemotePointerAxisEvent {
                        device,
                        time,
                        x: x.map(f64::from),
                        y: y.map(f64::from),
                        source: AxisSource::Wheel,
                        discrete: true,
                    },
                })
            }
        }
        Ok(())
    }
    fn release(&mut self, state: &mut State) {
        if self.retired {
            return;
        }
        self.retired = true;
        state.retire_pointer_device(&self.device);
        // EOF is observable only after the compositor has synchronously retired
        // this exact source, including consumed presses and owned grabs.
        let _ = self.writer.shutdown(Shutdown::Both);
    }
}
