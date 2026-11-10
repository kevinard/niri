//! Canopa's compositor-owned input leases. This is not a portal consent service.
//! The trusted portal owns grants and keeps its lease FD only while consent is active.
//! Closing that FD releases this source without synthesizing release-triggered binds.

use std::cell::RefCell;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use smithay::backend::input::{InputEvent, KeyState, Keycode};
use smithay::input::keyboard::{KeyboardSource, KeymapFileId};
use zbus::{fdo, interface, zvariant};

use super::Start;
use crate::input::remote_keyboard::{
    RemoteKeyboardBackend, RemoteKeyboardDevice, RemoteKeyboardKeyEvent,
};
use crate::niri::State;

mod pointer;
mod symbols;
mod wire;
use symbols::{Key, Planner};
use wire::{HeldKeys, Reader, TypedReader, FRAMES_PER_DISPATCH};

#[derive(Clone, Copy)]
pub enum Protocol {
    Evdev,
    Symbols,
}
enum InputReader {
    Evdev(Reader),
    Symbols(TypedReader),
}

pub type MappingDescriptors = Vec<(u64, String, (i32, i32), f64)>;

const MAX_LEASES: usize = 8;
const MAX_KEYMAP: usize = 1024 * 1024;
type Snapshot = (String, (u32, u32, u32, u32));

pub struct RemoteInput {
    to_niri: calloop::channel::SyncSender<Message>,
    admission: Arc<AtomicUsize>,
}

pub struct Permit(Arc<AtomicUsize>);
impl Permit {
    fn acquire(admission: &Arc<AtomicUsize>) -> fdo::Result<Self> {
        admission
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_LEASES).then_some(n + 1)
            })
            .map(|_| Self(admission.clone()))
            .map_err(|_| fdo::Error::LimitsExceeded("too many input leases".into()))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub enum Message {
    Open {
        stream: UnixStream,
        protocol: Protocol,
        permit: Permit,
        reply: async_channel::Sender<fdo::Result<Snapshot>>,
    },
    Snapshot {
        reply: async_channel::Sender<fdo::Result<Snapshot>>,
    },
    OpenMappedPointer {
        stream: UnixStream,
        permit: Permit,
        session: String,
        streams: Vec<String>,
        reply: async_channel::Sender<fdo::Result<(u32, MappingDescriptors)>>,
    },
    OpenPointer {
        stream: UnixStream,
        permit: Permit,
        reply: async_channel::Sender<fdo::Result<u32>>,
    },
}

impl RemoteInput {
    pub fn new(to_niri: calloop::channel::SyncSender<Message>) -> Self {
        Self {
            to_niri,
            admission: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn send(&self, msg: Message) -> fdo::Result<()> {
        self.to_niri
            .try_send(msg)
            .map_err(|_| fdo::Error::LimitsExceeded("input request queue unavailable".into()))
    }
    async fn open(
        &self,
        protocol: Protocol,
    ) -> fdo::Result<(zvariant::OwnedFd, String, (u32, u32, u32, u32))> {
        let permit = Permit::acquire(&self.admission)?;
        let (client, server) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        server
            .set_nonblocking(true)
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let (reply, receive) = async_channel::bounded(1);
        self.send(Message::Open {
            stream: server,
            protocol,
            permit,
            reply,
        })?;
        let (keymap, mods) = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok((zvariant::OwnedFd::from(OwnedFd::from(client)), keymap, mods))
    }
}

#[interface(name = "org.canopa.Niri.RemoteInput1")]
impl RemoteInput {
    /// Version 1: a keyboard-only lease, configured XKB keymap and current modifier masks.
    /// The returned socket accepts fixed eight-byte evdev/state frames; EOF revokes it.
    async fn open_keyboard(
        &self,
    ) -> fdo::Result<(zvariant::OwnedFd, String, (u32, u32, u32, u32))> {
        self.open(Protocol::Evdev).await
    }

    /// Typed commands resolve symbols on this compositor's input thread.
    /// The fixed command/reply formats are documented by remote_input::wire.
    async fn open_keyboard_symbols(
        &self,
    ) -> fdo::Result<(zvariant::OwnedFd, String, (u32, u32, u32, u32))> {
        self.open(Protocol::Symbols).await
    }

    #[zbus(property)]
    fn symbol_version(&self) -> u32 {
        1
    }

    /// FD-owned pointer commands with explicit acknowledgements. The capability
    /// mask excludes absolute motion, which needs an approved live cast mapping.
    async fn open_pointer(&self) -> fdo::Result<(zvariant::OwnedFd, u32)> {
        let permit = Permit::acquire(&self.admission)?;
        let (client, stream) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        stream
            .set_nonblocking(true)
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let (reply, receive) = async_channel::bounded(1);
        self.send(Message::OpenPointer {
            stream,
            permit,
            reply,
        })?;
        let caps = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok((zvariant::OwnedFd::from(OwnedFd::from(client)), caps))
    }

    /// Bind absolute input to fixed, started streams of this capture session.
    /// Region sizes and physical scales are immutable; changes retire the source.
    async fn open_mapped_pointer(
        &self,
        session: zvariant::ObjectPath<'_>,
        streams: Vec<zvariant::ObjectPath<'_>>,
    ) -> fdo::Result<(zvariant::OwnedFd, u32, MappingDescriptors)> {
        if streams.is_empty() || streams.len() > 16 {
            return Err(fdo::Error::InvalidArgs(
                "expected 1 to 16 capture streams".into(),
            ));
        }
        let permit = Permit::acquire(&self.admission)?;
        let (client, stream) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        stream
            .set_nonblocking(true)
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let (reply, receive) = async_channel::bounded(1);
        self.send(Message::OpenMappedPointer {
            stream,
            permit,
            session: session.to_string(),
            streams: streams.into_iter().map(|s| s.to_string()).collect(),
            reply,
        })?;
        let (caps, descriptors) = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok((
            zvariant::OwnedFd::from(OwnedFd::from(client)),
            caps,
            descriptors,
        ))
    }

    #[zbus(property)]
    fn mapped_pointer_version(&self) -> u32 {
        2
    }

    #[zbus(property)]
    fn pointer_version(&self) -> u32 {
        1
    }

    /// Read authoritative state even while the portal has no focused Wayland surface.
    async fn keyboard_snapshot(&self) -> fdo::Result<Snapshot> {
        let (reply, receive) = async_channel::bounded(1);
        self.send(Message::Snapshot { reply })?;
        receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))?
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
}

impl Start for RemoteInput {
    fn start(self, _monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        Ok(zbus::blocking::connection::Builder::session()?
            .name("org.canopa.Niri.RemoteInput1")?
            .serve_at("/org/canopa/Niri/RemoteInput1", self)?
            .build()?)
    }
}

fn snapshot(state: &mut State) -> fdo::Result<Snapshot> {
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    let map = keyboard.with_xkb_state(state, |ctx| ctx.xkb().lock().unwrap().keymap_as_string());
    if map.is_empty() || map.len() >= MAX_KEYMAP {
        return Err(fdo::Error::LimitsExceeded(
            "keymap exceeds lease limit".into(),
        ));
    }
    let m = keyboard.modifier_state().serialized;
    Ok((map, (m.depressed, m.latched, m.locked, m.layout_effective)))
}

#[derive(Default, Clone)]
pub struct Leases(Rc<RefCell<Vec<Weak<RefCell<Lease>>>>>, pointer::Leases);

/// Publish seat state after compositor work and before its clients are flushed.
/// Slow readers are retired instead of accumulating an unbounded output queue.
pub fn refresh(state: &mut State) {
    let Some(leases) = state
        .niri
        .dbus
        .as_ref()
        .map(|d| d.remote_input_leases.clone())
    else {
        return;
    };
    pointer::refresh(state, &leases.1);
    let active: Vec<_> = {
        let mut entries = leases.0.borrow_mut();
        entries.retain(|entry| entry.strong_count() != 0);
        entries.iter().filter_map(Weak::upgrade).collect()
    };
    if active.is_empty() {
        return;
    }
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    let keymap = keyboard.keymap_id();
    for lease in &active {
        let mut lease = lease.borrow_mut();
        if !lease.retired && lease.keymap != keymap {
            lease.release(state);
        }
    }
    for lease in active {
        let mut lease = lease.borrow_mut();
        if lease.retired {
            continue;
        }
        let m = keyboard.modifier_state().serialized;
        let mods = [m.depressed, m.latched, m.locked, m.layout_effective];
        if mods != lease.last_mods {
            if !lease.feedback(0, 0, mods) {
                lease.release(state);
            }
        }
    }
}

pub fn handle(state: &mut State, msg: Message, leases: &Leases) {
    match msg {
        Message::OpenMappedPointer {
            stream,
            permit,
            session,
            streams,
            reply,
        } => {
            pointer::open_mapped(state, stream, permit, reply, &leases.1, &session, &streams);
        }
        Message::OpenPointer {
            stream,
            permit,
            reply,
        } => {
            pointer::open(state, stream, permit, reply, &leases.1);
        }
        Message::Snapshot { reply } => {
            let _ = reply.try_send(snapshot(state));
        }
        Message::Open {
            stream,
            protocol,
            permit,
            reply,
        } => {
            state.restore_configured_keymap();
            let initial = match snapshot(state) {
                Ok(s) => s,
                Err(e) => {
                    let _ = reply.try_send(Err(e));
                    return;
                }
            };
            let writer = match stream.try_clone() {
                Ok(writer) => writer,
                Err(e) => {
                    let _ = reply.try_send(Err(fdo::Error::Failed(e.to_string())));
                    return;
                }
            };
            let keymap = state.niri.seat.get_keyboard().unwrap().keymap_id();
            let lease = Lease {
                _permit: permit,
                device: RemoteKeyboardDevice {
                    source: KeyboardSource::new_auxiliary(),
                },
                protocol,
                reader: match protocol {
                    Protocol::Evdev => InputReader::Evdev(Reader::default()),
                    Protocol::Symbols => InputReader::Symbols(TypedReader::default()),
                },
                symbols: Planner::default(),
                held: HeldKeys::default(),
                keymap,
                writer,
                last_mods: [initial.1 .0, initial.1 .1, initial.1 .2, initial.1 .3],
                retired: false,
            };
            let lease = Rc::new(RefCell::new(lease));
            leases.0.borrow_mut().push(Rc::downgrade(&lease));
            let result = state.niri.event_loop.insert_source(
                Generic::new(stream, Interest::READ, Mode::Level),
                move |_, socket, state| {
                    let mut lease = lease.borrow_mut();
                    if lease.retired {
                        return Ok(PostAction::Remove);
                    }
                    let mut socket = &**socket;
                    for _ in 0..FRAMES_PER_DISPATCH {
                        let event = match &mut lease.reader {
                            InputReader::Evdev(reader) => reader.next(&mut socket).map(|event| {
                                event.map(|(key, pressed)| (0, Key::Code(key), pressed))
                            }),
                            InputReader::Symbols(reader) => reader.next(&mut socket),
                        };
                        match event {
                            Ok(None) => return Ok(PostAction::Continue),
                            Ok(Some((sequence, key, pressed))) => {
                                if state.niri.seat.get_keyboard().unwrap().keymap_id()
                                    != lease.keymap
                                {
                                    lease.release(state);
                                    return Ok(PostAction::Remove);
                                }
                                let transitions = match lease.protocol {
                                    Protocol::Evdev => match key {
                                        Key::Code(code) => Ok(vec![(code, pressed)]),
                                        Key::Symbol(_) => unreachable!(),
                                    },
                                    Protocol::Symbols => {
                                        let keyboard = state.niri.seat.get_keyboard().unwrap();
                                        let m = keyboard.modifier_state().serialized;
                                        keyboard.with_xkb_state(state, |ctx| {
                                            let xkb = ctx.xkb().lock().unwrap();
                                            if pressed {
                                                // No reference or cloned map escapes the guard;
                                                // the planner retains only numeric key chords.
                                                lease.symbols.press(
                                                    unsafe { xkb.keymap() },
                                                    key,
                                                    [
                                                        m.depressed,
                                                        m.latched,
                                                        m.locked,
                                                        m.layout_effective,
                                                    ],
                                                )
                                            } else {
                                                lease.symbols.release(key)
                                            }
                                        })
                                    }
                                };
                                let status = match transitions {
                                    Ok(transitions) => {
                                        for (key, pressed) in transitions {
                                            if lease.held.transition(key, pressed).is_err() {
                                                lease.release(state);
                                                return Ok(PostAction::Remove);
                                            }
                                            state.process_input_event::<RemoteKeyboardBackend>(
                                                InputEvent::Keyboard {
                                                    event: RemoteKeyboardKeyEvent {
                                                        device: lease.device.clone(),
                                                        key,
                                                        state: if pressed {
                                                            KeyState::Pressed
                                                        } else {
                                                            KeyState::Released
                                                        },
                                                    },
                                                },
                                            );
                                        }
                                        0
                                    }
                                    Err(symbols::Error::Invalid) => 1,
                                    Err(symbols::Error::Unavailable) => 2,
                                    Err(symbols::Error::Conflict) => 3,
                                    Err(symbols::Error::Limit) => 4,
                                };
                                if sequence != 0 {
                                    let m = state
                                        .niri
                                        .seat
                                        .get_keyboard()
                                        .unwrap()
                                        .modifier_state()
                                        .serialized;
                                    if !lease.feedback(
                                        sequence,
                                        status,
                                        [m.depressed, m.latched, m.locked, m.layout_effective],
                                    ) {
                                        lease.release(state);
                                        return Ok(PostAction::Remove);
                                    }
                                }
                            }
                            Err(_) => {
                                lease.release(state);
                                return Ok(PostAction::Remove);
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                },
            );
            let response = result
                .map(|_| initial)
                .map_err(|e| fdo::Error::Failed(e.to_string()));
            let _ = reply.try_send(response);
        }
    }
}

struct Lease {
    _permit: Permit,
    device: RemoteKeyboardDevice,
    protocol: Protocol,
    reader: InputReader,
    symbols: Planner,
    held: HeldKeys,
    keymap: KeymapFileId,
    writer: UnixStream,
    last_mods: [u32; 4],
    retired: bool,
}
impl Lease {
    fn feedback(&mut self, sequence: u32, status: u32, mods: [u32; 4]) -> bool {
        let typed;
        let evdev;
        let bytes: &[u8] = match self.protocol {
            Protocol::Evdev => {
                evdev = mods
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>();
                &evdev
            }
            Protocol::Symbols => {
                typed = wire::feedback(sequence, status, mods);
                &typed
            }
        };
        if self.writer.write(bytes).ok() != Some(bytes.len()) {
            return false;
        }
        self.last_mods = mods;
        true
    }
    fn release(&mut self, state: &mut State) {
        if self.retired {
            return;
        }
        self.retired = true;
        let _ = self.writer.shutdown(Shutdown::Both);
        let keyboard = state.niri.seat.get_keyboard().unwrap();
        keyboard.release_source(state, self.device.source);
        crate::dbus::global_shortcuts::source_closed(state, self.device.source);
        let still_held = keyboard.pressed_keys();
        for key in self.held.keys() {
            let key = Keycode::from(*key + 8);
            if !still_held.contains(&key) {
                state.niri.pending_release_binds.remove(&key);
                state.niri.suppressed_keys.remove(&key);
            }
        }
        if !self.held.keys().is_empty() {
            if let Some(token) = state.niri.bind_repeat_timer.take() {
                state.niri.event_loop.remove(token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_is_bounded_and_recovers_after_drop() {
        let admission = Arc::new(AtomicUsize::new(0));
        let permits: Vec<_> = (0..MAX_LEASES)
            .map(|_| Permit::acquire(&admission).unwrap())
            .collect();
        assert!(Permit::acquire(&admission).is_err());
        drop(permits);
        assert_eq!(admission.load(Ordering::Relaxed), 0);
        assert!(Permit::acquire(&admission).is_ok());
    }
}
