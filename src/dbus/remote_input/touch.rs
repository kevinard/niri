//! Capture-bound, FD-owned touch. Physical events reserve their own slots first;
//! closing a lease lifts only its contacts and flushes a frame before slot reuse.
use std::any::TypeId;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use smithay::backend::input::{Device, InputEvent, InputTime, TouchSlot};
use zbus::fdo;

use super::{MappingDescriptors, Permit};
use crate::input::remote_touch::contacts::{Registry, Retired, Source, SEAT_SLOTS};
use crate::input::remote_touch::{
    Backend, Device as RemoteDevice, FrameEvent, PositionEvent, UpEvent,
};
use crate::niri::State;
use crate::screencasting::sources::mapping::Bindings;

mod wire;
use wire::{Command, Reader, COMMANDS_PER_DISPATCH};

#[derive(Default)]
struct Ownership {
    registry: Registry,
    leases: BTreeMap<Source, Weak<RefCell<Lease>>>,
    physical: BTreeMap<(TypeId, String), BTreeSet<i32>>,
    physical_devices: BTreeSet<(TypeId, String)>,
}
#[derive(Default, Clone)]
pub struct Leases(Rc<RefCell<Ownership>>);

fn leases(state: &State) -> Option<Leases> {
    state
        .niri
        .dbus
        .as_ref()
        .map(|d| d.remote_input_leases.2.clone())
}

pub fn has_devices(state: &State) -> bool {
    leases(state).is_some_and(|leases| {
        let ownership = leases.0.borrow();
        !ownership.leases.is_empty() || !ownership.physical_devices.is_empty()
    })
}

/// Whether any remote touch lease is open.
pub fn has_leases(state: &State) -> bool {
    leases(state).is_some_and(|leases| !leases.0.borrow().leases.is_empty())
}

fn update_capability(state: &mut State, leases: &Leases) {
    let ownership = leases.0.borrow();
    if !ownership.leases.is_empty()
        || !ownership.physical_devices.is_empty()
        || !state.niri.touch.is_empty()
    {
        if state.niri.seat.get_touch().is_none() {
            state.niri.seat.add_touch();
        }
    } else if state.niri.touch.is_empty() {
        state.niri.seat.remove_touch();
    }
}

pub fn add_device<D: Device + 'static>(state: &State, device: &D) {
    if !device.has_capability(smithay::backend::input::DeviceCapability::Touch) {
        return;
    }
    if let Some(leases) = leases(state) {
        leases
            .0
            .borrow_mut()
            .physical_devices
            .insert((TypeId::of::<D>(), device.id()));
    }
}

fn finish_retired(state: &mut State, leases: &Leases, retired: Retired) {
    let lease = leases
        .0
        .borrow_mut()
        .leases
        .remove(&retired.source)
        .and_then(|w| w.upgrade());
    if let Some(lease) = lease {
        let mut lease = lease.borrow_mut();
        lease.retired = true;
        let _ = lease.writer.shutdown(Shutdown::Both);
    }
    let slots: Vec<_> = retired
        .slots
        .into_iter()
        .map(|slot| TouchSlot::from(Some(slot.get())))
        .collect();
    state.retire_touch_slots(&slots);
}

pub fn refresh(state: &mut State, leases: &Leases) {
    let active: Vec<_> = leases
        .0
        .borrow()
        .leases
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    for lease in active {
        let mut lease = lease.borrow_mut();
        if state.niri.is_locked() || !lease.bindings.valid(&state.niri) {
            lease.release(state, leases);
        }
    }
    update_capability(state, leases);
}

/// Returns before physical down is delivered, including the old owner's up/frame.
pub fn physical_down<D: Device + 'static>(state: &mut State, device: &D, slot: TouchSlot) {
    let Some(leases) = leases(state) else {
        return;
    };
    let slot = i32::from(slot);
    if !(0..SEAT_SLOTS as i32).contains(&slot) {
        return;
    }
    let retired = {
        let mut ownership = leases.0.borrow_mut();
        let slots = ownership
            .physical
            .entry((TypeId::of::<D>(), device.id()))
            .or_default();
        if !slots.insert(slot) {
            return;
        }
        ownership.registry.physical_down(slot)
    };
    if let Some(retired) = retired {
        finish_retired(state, &leases, retired);
    }
    // The physical caller still needs this handle to deliver its down, even
    // after it retired the final remote lease on a nested backend.
}

pub fn physical_motion<D: Device + 'static>(state: &State, device: &D, slot: TouchSlot) -> bool {
    let Some(leases) = leases(state) else {
        return true;
    };
    let slot = i32::from(slot);
    if !(0..SEAT_SLOTS as i32).contains(&slot) {
        return true;
    }
    let ownership = leases.0.borrow();
    ownership
        .physical
        .get(&(TypeId::of::<D>(), device.id()))
        .is_some_and(|slots| slots.contains(&slot))
        && ownership.registry.physical_motion_allowed(slot)
}

pub fn physical_up<D: Device + 'static>(state: &State, device: &D, slot: TouchSlot) -> bool {
    let Some(leases) = leases(state) else {
        return true;
    };
    let slot = i32::from(slot);
    if !(0..SEAT_SLOTS as i32).contains(&slot) {
        return true;
    }
    let mut ownership = leases.0.borrow_mut();
    let key = (TypeId::of::<D>(), device.id());
    let Some(slots) = ownership.physical.get_mut(&key) else {
        return false;
    };
    if !slots.remove(&slot) {
        return false;
    }
    if slots.is_empty() {
        ownership.physical.remove(&key);
    }
    ownership.registry.physical_up(slot)
}

pub fn physical_cancel(state: &mut State) {
    let Some(leases) = leases(state) else {
        return;
    };
    let retired = {
        let mut ownership = leases.0.borrow_mut();
        ownership.physical.clear();
        ownership.registry.physical_cancel()
    };
    for retired in retired {
        finish_retired(state, &leases, retired);
    }
}

pub fn remove_device<D: Device + 'static>(state: &mut State, device: &D) {
    let Some(leases) = leases(state) else {
        return;
    };
    let slots = {
        let mut ownership = leases.0.borrow_mut();
        ownership
            .physical_devices
            .remove(&(TypeId::of::<D>(), device.id()));
        let slots = ownership
            .physical
            .remove(&(TypeId::of::<D>(), device.id()))
            .unwrap_or_default();
        slots
            .into_iter()
            .filter_map(|slot| {
                ownership.registry.physical_up(slot);
                // Another physical device can still own this raw slot.
                (!ownership.registry.physical_motion_allowed(slot))
                    .then_some(TouchSlot::from(Some(slot as u32)))
            })
            .collect::<Vec<_>>()
    };
    if !slots.is_empty() {
        state.retire_touch_slots(&slots);
    }
}

pub fn open_mapped(
    state: &mut State,
    stream: UnixStream,
    permit: Permit,
    reply: async_channel::Sender<fdo::Result<MappingDescriptors>>,
    leases: &Leases,
    session: &str,
    streams: &[String],
) {
    let result = (|| {
        if state.niri.is_locked() {
            return Err(fdo::Error::AccessDenied("session is locked".into()));
        }
        let bindings = Bindings::new(&state.niri, session, streams)?;
        let descriptors = bindings.descriptors();
        let writer = stream
            .try_clone()
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let source = leases
            .0
            .borrow_mut()
            .registry
            .open()
            .map_err(|_| fdo::Error::LimitsExceeded("touch source limit".into()))?;
        let lease = Rc::new(RefCell::new(Lease {
            _permit: permit,
            source,
            bindings,
            writer,
            reader: Reader::default(),
            retired: false,
        }));
        leases
            .0
            .borrow_mut()
            .leases
            .insert(source, Rc::downgrade(&lease));
        update_capability(state, leases);
        let live = lease.clone();
        let owners = leases.clone();
        let inserted = state.niri.event_loop.insert_source(
            Generic::new(stream, Interest::READ, Mode::Level),
            move |_, socket, state| {
                let mut lease = live.borrow_mut();
                if lease.retired {
                    return Ok(PostAction::Remove);
                }
                if state.niri.is_locked() || !lease.bindings.valid(&state.niri) {
                    lease.release(state, &owners);
                    return Ok(PostAction::Remove);
                }
                let mut socket = &**socket;
                for _ in 0..COMMANDS_PER_DISPATCH {
                    let (sequence, command) = match lease.reader.next(&mut socket) {
                        Ok(Some(command)) => command,
                        Ok(None) => return Ok(PostAction::Continue),
                        Err(_) => {
                            lease.release(state, &owners);
                            return Ok(PostAction::Remove);
                        }
                    };
                    if lease.dispatch(state, &owners, command).is_err()
                        || lease.writer.write(&wire::feedback(sequence)).ok() != Some(16)
                    {
                        lease.release(state, &owners);
                        return Ok(PostAction::Remove);
                    }
                }
                Ok(PostAction::Continue)
            },
        );
        if let Err(error) = inserted {
            lease.borrow_mut().release(state, leases);
            return Err(fdo::Error::Failed(error.to_string()));
        }
        if reply.try_send(Ok(descriptors)).is_err() {
            lease.borrow_mut().release(state, leases);
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = reply.try_send(Err(error));
    }
}

struct Lease {
    _permit: Permit,
    source: Source,
    bindings: Bindings,
    writer: UnixStream,
    reader: Reader,
    retired: bool,
}
impl Lease {
    fn dispatch(
        &mut self,
        state: &mut State,
        leases: &Leases,
        command: Command,
    ) -> std::io::Result<()> {
        let invalid = std::io::ErrorKind::InvalidData;
        state.niri.advance_animations();
        let time = InputTime::now();
        let device = RemoteDevice(self.source);
        match command {
            Command::Frame => {
                state.process_input_event::<Backend>(InputEvent::TouchFrame {
                    event: FrameEvent { device, time },
                });
            }
            Command::Down {
                contact,
                stream,
                x,
                y,
            } => {
                let point = self
                    .bindings
                    .point(&state.niri, stream, x, y)
                    .ok_or(invalid)?;
                let slot = leases
                    .0
                    .borrow_mut()
                    .registry
                    .down(self.source, contact)
                    .map_err(|_| invalid)?;
                state.process_input_event::<Backend>(InputEvent::TouchDown {
                    event: PositionEvent {
                        device,
                        time,
                        slot: TouchSlot::from(Some(slot.get())),
                        position: point,
                    },
                });
            }
            Command::Motion {
                contact,
                stream,
                x,
                y,
            } => {
                let point = self
                    .bindings
                    .point(&state.niri, stream, x, y)
                    .ok_or(invalid)?;
                let slot = leases
                    .0
                    .borrow()
                    .registry
                    .motion(self.source, contact)
                    .map_err(|_| invalid)?;
                state.process_input_event::<Backend>(InputEvent::TouchMotion {
                    event: PositionEvent {
                        device,
                        time,
                        slot: TouchSlot::from(Some(slot.get())),
                        position: point,
                    },
                });
            }
            Command::Up { contact } => {
                let slot = leases
                    .0
                    .borrow_mut()
                    .registry
                    .up(self.source, contact)
                    .map_err(|_| invalid)?;
                state.process_input_event::<Backend>(InputEvent::TouchUp {
                    event: UpEvent {
                        device,
                        time,
                        slot: TouchSlot::from(Some(slot.get())),
                    },
                });
            }
        }
        Ok(())
    }
    fn release(&mut self, state: &mut State, leases: &Leases) {
        if self.retired {
            return;
        }
        self.retired = true;
        let retired = {
            let mut ownership = leases.0.borrow_mut();
            ownership.leases.remove(&self.source);
            ownership.registry.retire(self.source)
        };
        if let Some(retired) = retired {
            let slots: Vec<_> = retired
                .slots
                .into_iter()
                .map(|slot| TouchSlot::from(Some(slot.get())))
                .collect();
            state.retire_touch_slots(&slots);
        }
        let _ = self.writer.shutdown(Shutdown::Both);
        update_capability(state, leases);
    }
}
