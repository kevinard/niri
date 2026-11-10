//! Private compositor capture lease. Consent and EI translation belong to the
//! portal; this FD is a control capability and must never be handed to an app.
//! Version 2 adds independently granted keyboard capture. Touch and public XDG
//! session/consent handling remain outside this private compositor service.
use std::cell::{Cell, RefCell};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use serde::{Deserialize, Serialize};
use smithay::input::keyboard::{KeyboardSource, KeymapFileId};
use zbus::{fdo, interface, zvariant};

use super::Start;
use crate::niri::{PointerVisibility, State};

mod geometry;
mod keyboard;
mod keys;
mod masks;
use geometry::{Barrier, Space, Zone};

const MAX_LEASES: usize = 8;
const MAX_MESSAGE: usize = 16 * 1024;
type Zones = Vec<(u32, u32, i32, i32)>;
type Initial = (u32, Zones, u32, String);
type Reply = async_channel::Sender<fdo::Result<Initial>>;
pub enum Message {
    Open(Open),
    Snapshot(async_channel::Sender<fdo::Result<(u32, String)>>),
}
pub struct InputCapture {
    to_niri: calloop::channel::SyncSender<Message>,
    admission: Arc<AtomicUsize>,
}
pub struct Permit(Arc<AtomicUsize>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
pub struct Open {
    stream: UnixStream,
    permit: Permit,
    reply: Reply,
    grants: u32,
}
impl InputCapture {
    pub fn new(to_niri: calloop::channel::SyncSender<Message>) -> Self {
        Self {
            to_niri,
            admission: Arc::new(AtomicUsize::new(0)),
        }
    }
    async fn open(&self, grants: u32) -> fdo::Result<(zvariant::OwnedFd, u32, Zones, u32, String)> {
        if grants == 0 || grants & !3 != 0 {
            return Err(fdo::Error::InvalidArgs(
                "invalid capture capabilities".into(),
            ));
        }
        self.admission
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_LEASES).then_some(n + 1)
            })
            .map_err(|_| fdo::Error::LimitsExceeded("too many capture leases".into()))?;
        let permit = Permit(self.admission.clone());
        let (client, stream) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        stream
            .set_nonblocking(true)
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let (reply, receive) = async_channel::bounded(1);
        self.to_niri
            .try_send(Message::Open(Open {
                stream,
                permit,
                reply,
                grants,
            }))
            .map_err(|_| fdo::Error::LimitsExceeded("capture admission unavailable".into()))?;
        let (generation, zones, keymap, text) = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok((
            zvariant::OwnedFd::from(OwnedFd::from(client)),
            generation,
            zones,
            keymap,
            text,
        ))
    }
}
#[interface(name = "org.canopa.Niri.InputCapture1")]
impl InputCapture {
    async fn open_pointer(&self) -> fdo::Result<(zvariant::OwnedFd, u32, Zones)> {
        let (fd, generation, zones, _, _) = self.open(2).await?;
        Ok((fd, generation, zones))
    }
    async fn open_capture(
        &self,
        capabilities: u32,
    ) -> fdo::Result<(zvariant::OwnedFd, u32, Zones, u32, String)> {
        self.open(capabilities).await
    }
    async fn keyboard_snapshot(&self) -> fdo::Result<(u32, String)> {
        let (reply, receive) = async_channel::bounded(1);
        self.to_niri
            .try_send(Message::Snapshot(reply))
            .map_err(|_| fdo::Error::LimitsExceeded("capture snapshot unavailable".into()))?;
        receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))?
    }
    #[zbus(property)]
    fn version(&self) -> u32 {
        2
    }
    #[zbus(property)]
    fn supported_capabilities(&self) -> u32 {
        3
    }
}
impl Start for InputCapture {
    fn start(self, _monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        Ok(zbus::blocking::connection::Builder::session()?
            .name("org.canopa.Niri.InputCapture1")?
            .serve_at("/org/canopa/Niri/InputCapture1", self)?
            .build()?)
    }
}

#[derive(Default, Clone)]
pub struct Leases {
    entries: Rc<RefCell<Vec<Weak<RefCell<Lease>>>>>,
    hidden: Rc<Cell<bool>>,
    keyboard: Rc<RefCell<keyboard::Keyboard>>,
    next: Rc<Cell<u64>>,
    map: Rc<Cell<Option<KeymapFileId>>>,
    map_generation: Rc<Cell<u32>>,
    masks: Rc<RefCell<masks::Masks>>,
}
impl Leases {
    fn live(&self) -> Vec<Rc<RefCell<Lease>>> {
        let mut entries = self.entries.borrow_mut();
        entries.retain(|entry| entry.strong_count() != 0);
        entries.iter().filter_map(Weak::upgrade).collect()
    }
    fn active(&self) -> Option<Rc<RefCell<Lease>>> {
        self.live()
            .into_iter()
            .find(|lease| lease.borrow().active.is_some())
    }
    fn sync_cursor(&self, state: &mut State) {
        if self
            .active()
            .is_some_and(|lease| lease.borrow().grants & 2 != 0)
        {
            self.hidden.set(true);
            if state.niri.pointer_visibility != PointerVisibility::Hidden {
                state.niri.pointer_visibility = PointerVisibility::Hidden;
                state.niri.queue_redraw_all();
            }
        } else if self.hidden.replace(false) {
            state.niri.pointer_visibility = PointerVisibility::Visible;
            state.niri.queue_redraw_all();
        }
    }
}
fn leases(state: &State) -> Option<Leases> {
    state
        .niri
        .dbus
        .as_ref()
        .map(|dbus| dbus.input_capture_leases.clone())
}
fn zones(state: &State) -> Vec<Zone> {
    state
        .niri
        .global_space
        .outputs()
        .filter_map(|output| {
            let rect = state.niri.global_space.output_geometry(output)?;
            Some(Zone {
                width: rect.size.w.try_into().unwrap_or(0),
                height: rect.size.h.try_into().unwrap_or(0),
                x: rect.loc.x,
                y: rect.loc.y,
            })
        })
        .collect()
}
fn describe(space: &Space) -> Zones {
    space
        .zones()
        .iter()
        .map(|z| (z.width, z.height, z.x, z.y))
        .collect()
}
fn unavailable(state: &State) -> bool {
    state.niri.is_locked()
        || state.niri.screenshot_ui.is_open()
        || state.niri.exit_confirm_dialog.is_open()
}

/// Messages have a little-endian u32 byte count followed by bounded JSON.
/// A partial/oversized/malformed request or failed event write retires the lease.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    sequence: u64,
    command: Command,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    GetZones,
    SetBarriers {
        zone_set: u32,
        barriers: Vec<(u32, (i32, i32, i32, i32))>,
    },
    Enable {
        #[serde(default)]
        keymap: Option<u32>,
    },
    Disable,
    Release {
        activation: u32,
    },
}
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Reply {
        sequence: u64,
        error: Option<&'static str>,
        failed_barriers: Vec<u32>,
    },
    Zones {
        sequence: u64,
        zone_set: u32,
        zones: Zones,
    },
    ZonesChanged {
        zone_set: u32,
    },
    Activated {
        activation: u32,
        barrier: u32,
        position: (f64, f64),
    },
    Deactivated {
        activation: u32,
    },
    Disabled,
    KeymapChanged {
        keymap: u32,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    Key {
        time: u64,
        code: u32,
        pressed: bool,
    },
    Motion {
        time: u64,
        delta: (f64, f64),
    },
    Button {
        time: u64,
        button: u32,
        pressed: bool,
    },
    Axis {
        time: u64,
        source: u32,
        amount: (Option<f64>, Option<f64>),
        v120: (Option<f64>, Option<f64>),
    },
}
#[derive(Default)]
struct Reader {
    header: [u8; 4],
    header_bytes: usize,
    payload: Vec<u8>,
    payload_bytes: usize,
}
impl Reader {
    fn next(&mut self, stream: &mut impl Read) -> io::Result<Option<Request>> {
        loop {
            let target = if self.header_bytes < 4 {
                &mut self.header[self.header_bytes..]
            } else {
                &mut self.payload[self.payload_bytes..]
            };
            match stream.read(target) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    if self.header_bytes < 4 {
                        self.header_bytes += n;
                        if self.header_bytes == 4 {
                            let len = u32::from_le_bytes(self.header) as usize;
                            if len == 0 || len > MAX_MESSAGE {
                                return Err(io::ErrorKind::InvalidData.into());
                            }
                            self.payload.resize(len, 0);
                        }
                    } else {
                        self.payload_bytes += n;
                        if self.payload_bytes == self.payload.len() {
                            let value = serde_json::from_slice(&self.payload)
                                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                            self.header_bytes = 0;
                            self.payload_bytes = 0;
                            self.payload.clear();
                            return Ok(Some(value));
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}
struct Lease {
    id: u64,
    grants: u32,
    keyboard: Rc<RefCell<keyboard::Keyboard>>,
    key_state: Option<keys::State>,
    masks: Rc<RefCell<masks::Masks>>,
    last_time: u64,
    _permit: Permit,
    reader: Reader,
    writer: UnixStream,
    space: Space,
    enabled: bool,
    active: Option<u32>,
    activation: u32,
    position: (f64, f64),
    sequence: u64,
    retired: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.retire();
    }
}
impl Lease {
    fn end_keyboard(&mut self) {
        if self
            .keyboard
            .borrow()
            .active()
            .is_some_and(|owner| owner.lease == self.id)
        {
            // The EI receiver lifts its owned keys before stop_emulating on
            // Deactivated or EOF. Keep physical tails suppressed in the producer.
            self.keyboard.borrow_mut().deactivate();
            self.masks
                .borrow_mut()
                .retire(|source| self.keyboard.borrow().has_suppressed_source(source));
        }
        self.key_state = None;
    }
    fn retire(&mut self) {
        self.end_keyboard();
        self.enabled = false;
        self.active = None;
        self.retired = true;
        let _ = self.writer.shutdown(Shutdown::Both);
    }
    fn feedback(&mut self, mut event: Event) -> bool {
        match &mut event {
            Event::Motion { time, .. }
            | Event::Button { time, .. }
            | Event::Axis { time, .. }
            | Event::Key { time, .. } => {
                *time = (*time).max(self.last_time);
                self.last_time = *time;
            }
            _ => {}
        }
        if self.retired {
            return false;
        }
        let result = (|| -> io::Result<()> {
            let payload = serde_json::to_vec(&event)?;
            if payload.len() > MAX_MESSAGE {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut packet = Vec::with_capacity(4 + payload.len());
            packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            packet.extend_from_slice(&payload);
            self.writer.write_all(&packet)
        })();
        if result.is_err() {
            self.retire();
            false
        } else {
            true
        }
    }
    fn deactivate(&mut self) {
        if let Some(activation) = self.active.take() {
            self.end_keyboard();
            self.feedback(Event::Deactivated { activation });
        }
    }
    fn publish_modifiers(&mut self) -> bool {
        let Some(keys) = &self.key_state else {
            return true;
        };
        let (depressed, latched, locked, group) = keys.modifiers();
        self.feedback(Event::Modifiers {
            depressed,
            latched,
            locked,
            group,
        })
    }
    fn activate(&mut self, state: &mut State, barrier: u32, position: (f64, f64)) -> bool {
        self.activation = self.activation.wrapping_add(1).max(1);
        let activation = self.activation;
        if self.grants & 1 != 0 {
            let local = self.keyboard.borrow().local_keys().collect::<Vec<_>>();
            let keys = keys::State::new(
                state,
                local.into_iter(),
                self.masks.borrow().local_depressed(),
            );
            let owner = keyboard::Recipient {
                lease: self.id,
                activation,
            };
            let admitted = self.keyboard.borrow_mut().activate(owner);
            if admitted.is_err() {
                self.disable();
                return false;
            }
            let masks_admitted = self.masks.borrow_mut().activate();
            if masks_admitted.is_err() {
                self.disable();
                return false;
            }
            self.key_state = Some(keys);
            if let Some(token) = state.niri.bind_repeat_timer.take() {
                state.niri.event_loop.remove(token);
            }
        }
        self.active = Some(activation);
        self.position = position;
        let ready = self.feedback(Event::Activated {
            activation,
            barrier,
            position,
        }) && self.publish_modifiers();
        ready && self.grants & 2 != 0
    }
    fn deliver_key(&mut self, outcome: keyboard::Outcome, time: u64) {
        let before = self.key_state.as_ref().map(keys::State::modifiers);
        if let (Some(state), Some(key)) = (&mut self.key_state, outcome.physical) {
            state.update(key);
        }
        if let Some((owner, key)) = outcome.captured {
            if owner.lease == self.id && self.active == Some(owner.activation) {
                self.feedback(Event::Key {
                    time,
                    code: key.code,
                    pressed: key.pressed,
                });
            }
        }
        if self.key_state.as_ref().map(keys::State::modifiers) != before {
            self.publish_modifiers();
        }
    }
    fn disable(&mut self) {
        self.deactivate();
        if std::mem::take(&mut self.enabled) {
            self.feedback(Event::Disabled);
        }
    }
    fn command(&mut self, request: Request, busy: bool, available: bool, keymap: u32) {
        if request.sequence == 0 || request.sequence <= self.sequence {
            self.retire();
            return;
        }
        self.sequence = request.sequence;
        let mut error = None;
        let mut failed_barriers = Vec::new();
        match request.command {
            Command::GetZones => {
                self.feedback(Event::Zones {
                    sequence: request.sequence,
                    zone_set: self.space.generation(),
                    zones: describe(&self.space),
                });
                return;
            }
            Command::SetBarriers { zone_set, barriers } => {
                let barriers: Vec<_> = barriers
                    .into_iter()
                    .map(|(id, position)| Barrier { id, position })
                    .collect();
                match self.space.replace(zone_set, &barriers) {
                    Ok(failed) => {
                        self.disable();
                        failed_barriers = failed;
                    }
                    Err(_) => error = Some("invalid_barriers_or_zone_set"),
                }
            }
            Command::Enable { keymap: approved } => {
                if self.grants & 1 != 0 && !self.keyboard.borrow().admissible() {
                    error = Some("keyboard_admission_exhausted");
                } else if self.grants & 1 != 0 && approved != Some(keymap) {
                    error = Some("stale_keyboard_snapshot");
                } else if !available {
                    error = Some("unavailable");
                } else if busy {
                    error = Some("busy");
                } else {
                    self.enabled = true;
                }
            }
            Command::Disable => self.disable(),
            Command::Release { activation } => {
                if self.active != Some(activation) {
                    error = Some("stale_activation");
                } else {
                    self.deactivate();
                }
            }
        }
        self.feedback(Event::Reply {
            sequence: request.sequence,
            error,
            failed_barriers,
        });
    }
}

pub fn handle(state: &mut State, message: Message, entries: &Leases) {
    refresh(state);
    match message {
        Message::Open(message) => open(state, message, entries),
        Message::Snapshot(reply) => {
            let _ = reply
                .try_send(keys::snapshot(state).map(|text| (entries.map_generation.get(), text)));
        }
    }
}
fn open(state: &mut State, message: Open, entries: &Leases) {
    if unavailable(state) {
        let _ = message
            .reply
            .try_send(Err(fdo::Error::AccessDenied("capture unavailable".into())));
        return;
    }
    let space = match Space::new(zones(state)) {
        Ok(space) => space,
        Err(_) => {
            let _ = message
                .reply
                .try_send(Err(fdo::Error::LimitsExceeded("invalid zones".into())));
            return;
        }
    };
    let keymap = if message.grants & 1 != 0 {
        match keys::snapshot(state) {
            Ok(text) => text,
            Err(error) => {
                let _ = message.reply.try_send(Err(error));
                return;
            }
        }
    } else {
        String::new()
    };
    let Some(id) = entries.next.get().checked_add(1) else {
        let _ = message.reply.try_send(Err(fdo::Error::LimitsExceeded(
            "capture identity exhausted".into(),
        )));
        return;
    };
    entries.next.set(id);
    let initial = (
        space.generation(),
        describe(&space),
        entries.map_generation.get(),
        keymap,
    );
    let writer = match message.stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            let _ = message
                .reply
                .try_send(Err(fdo::Error::Failed(error.to_string())));
            return;
        }
    };
    let lease = Rc::new(RefCell::new(Lease {
        id,
        grants: message.grants,
        keyboard: entries.keyboard.clone(),
        key_state: None,
        masks: entries.masks.clone(),
        last_time: 0,
        _permit: message.permit,
        reader: Reader::default(),
        writer,
        space,
        enabled: false,
        active: None,
        activation: 0,
        position: (0., 0.),
        sequence: 0,
        retired: false,
    }));
    entries.entries.borrow_mut().push(Rc::downgrade(&lease));
    let registry = entries.clone();
    let result = state.niri.event_loop.insert_source(
        Generic::new(message.stream, Interest::READ, Mode::Level),
        move |_, socket, state| {
            refresh(state);
            for _ in 0..8 {
                if lease.borrow().retired {
                    registry.sync_cursor(state);
                    return Ok(PostAction::Remove);
                }
                let reading = { lease.borrow_mut().reader.next(&mut &**socket) };
                let request = match reading {
                    Ok(Some(request)) => request,
                    Ok(None) => {
                        registry.sync_cursor(state);
                        return Ok(PostAction::Continue);
                    }
                    Err(_) => {
                        lease.borrow_mut().retire();
                        registry.sync_cursor(state);
                        return Ok(PostAction::Remove);
                    }
                };
                let busy = registry
                    .live()
                    .iter()
                    .any(|other| !Rc::ptr_eq(other, &lease) && other.borrow().enabled);
                lease.borrow_mut().command(
                    request,
                    busy,
                    !unavailable(state),
                    registry.map_generation.get(),
                );
            }
            registry.sync_cursor(state);
            Ok(PostAction::Continue)
        },
    );
    let _ = message.reply.try_send(
        result
            .map(|_| initial)
            .map_err(|error| fdo::Error::Failed(error.to_string())),
    );
}

pub fn refresh(state: &mut State) {
    let Some(registry) = leases(state) else {
        return;
    };
    let zones = zones(state);
    let map = state.niri.seat.get_keyboard().unwrap().keymap_id();
    let changed_map = registry
        .map
        .replace(Some(map))
        .is_some_and(|previous| previous != map);
    if registry.map_generation.get() == 0 {
        registry.map_generation.set(1);
    } else if changed_map {
        registry
            .map_generation
            .set(registry.map_generation.get().wrapping_add(1).max(1));
    }
    for lease in registry.live() {
        let mut lease = lease.borrow_mut();
        if lease.retired {
            continue;
        }
        if unavailable(state) {
            lease.disable();
        }
        if changed_map && lease.grants & 1 != 0 {
            lease.disable();
            lease.feedback(Event::KeymapChanged {
                keymap: registry.map_generation.get(),
            });
        }
        if let Some(keys) = lease.key_state.as_mut() {
            if keys.sync_external(state) {
                lease.publish_modifiers();
            }
        }
        match lease.space.refresh(zones.clone()) {
            Ok(Some(zone_set)) => {
                lease.disable();
                lease.feedback(Event::ZonesChanged { zone_set });
            }
            Ok(None) => {}
            Err(_) => lease.retire(),
        }
    }
    registry.sync_cursor(state);
}
pub fn disable(state: &mut State) -> bool {
    let Some(registry) = leases(state) else {
        return false;
    };
    let mut changed = false;
    for lease in registry.live() {
        let mut lease = lease.borrow_mut();
        changed |= lease.enabled;
        lease.disable();
    }
    registry.sync_cursor(state);
    changed
}
/// Absolute events carry their unconfined global position; relative events carry
/// a delta. The barrier-triggering motion is described by Activated.position;
/// subsequent motions are emitted once, with no ordinary local delivery.
pub fn motion(
    state: &mut State,
    from: (f64, f64),
    value: (f64, f64),
    absolute: bool,
    time: u64,
    eligible: bool,
) -> bool {
    refresh(state);
    let Some(registry) = leases(state) else {
        return false;
    };
    if !eligible {
        disable(state);
        return false;
    }
    let Some(lease) = registry
        .live()
        .into_iter()
        .find(|lease| lease.borrow().enabled)
    else {
        return false;
    };
    let mut lease = lease.borrow_mut();
    let to = if absolute {
        value
    } else {
        (from.0 + value.0, from.1 + value.1)
    };
    if ![to.0, to.1, value.0, value.1]
        .into_iter()
        .all(f64::is_finite)
    {
        lease.retire();
        drop(lease);
        registry.sync_cursor(state);
        return false;
    }
    let consumed = if lease.active.is_some() {
        if lease.grants & 2 == 0 {
            return false;
        }
        let delta = if absolute {
            (value.0 - lease.position.0, value.1 - lease.position.1)
        } else {
            value
        };
        lease.position = if absolute {
            value
        } else {
            (lease.position.0 + value.0, lease.position.1 + value.1)
        };
        if [delta.0, delta.1, lease.position.0, lease.position.1]
            .into_iter()
            .all(f64::is_finite)
        {
            lease.feedback(Event::Motion { time, delta })
        } else {
            lease.retire();
            false
        }
    } else if let Some(barrier) = lease
        .space
        .crossing(from, to)
        .filter(|_| state.niri.suppressed_buttons.is_empty())
    {
        lease.activate(state, barrier, to)
    } else {
        false
    };
    drop(lease);
    registry.sync_cursor(state);
    consumed
}
pub fn button(state: &mut State, button: u32, pressed: bool, time: u64) -> bool {
    refresh(state);
    let Some(registry) = leases(state) else {
        return false;
    };
    let Some(lease) = registry.active() else {
        return false;
    };
    if lease.borrow().grants & 2 == 0 {
        return false;
    }
    let consumed = lease.borrow_mut().feedback(Event::Button {
        time,
        button,
        pressed,
    });
    registry.sync_cursor(state);
    consumed
}
pub fn axis(
    state: &mut State,
    time: u64,
    source: u32,
    amount: (Option<f64>, Option<f64>),
    v120: (Option<f64>, Option<f64>),
) -> bool {
    refresh(state);
    let Some(registry) = leases(state) else {
        return false;
    };
    let Some(lease) = registry.active() else {
        return false;
    };
    let mut lease = lease.borrow_mut();
    if lease.grants & 2 == 0 {
        return false;
    }
    let consumed = if [amount.0, amount.1, v120.0, v120.1]
        .into_iter()
        .flatten()
        .all(f64::is_finite)
    {
        lease.feedback(Event::Axis {
            time,
            source,
            amount,
            v120,
        })
    } else {
        lease.retire();
        false
    };
    drop(lease);
    registry.sync_cursor(state);
    consumed
}

/// Return true only when the event must bypass ordinary seat XKB and bindings.
/// Protected accessibility/shortcut presses remain local; already-owned tails
/// keep their original route even after that policy changes.
pub fn key(
    state: &mut State,
    source: KeyboardSource,
    code: u32,
    pressed: bool,
    time: u64,
    local: bool,
    protected: bool,
) -> bool {
    refresh(state);
    let Some(registry) = leases(state) else {
        return false;
    };
    let Some(source) = super::global_shortcuts::source_id(source) else {
        disable(state);
        return false;
    };
    if local
        && pressed
        && code == 1
        && registry.active().is_some_and(|lease| {
            lease
                .borrow()
                .key_state
                .as_ref()
                .is_some_and(keys::State::pause_combo)
        })
    {
        disable(state);
        return registry
            .keyboard
            .borrow_mut()
            .suppress_press(source, code)
            .is_ok();
    }
    let was_tail = registry.keyboard.borrow().has_suppressed_source(source);
    let result = registry
        .keyboard
        .borrow_mut()
        .edge(source, code, pressed, local && !protected);
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(_) => {
            disable_keyboard(state);
            return false;
        }
    };
    let raw_tail = registry.keyboard.borrow().has_suppressed_source(source);
    if let Some(lease) = registry.active() {
        if lease.borrow().grants & 1 != 0
            && outcome.route == keyboard::Route::Intercept
            && (!was_tail || outcome.captured.is_some())
        {
            let recorded = registry.masks.borrow_mut().record_capture(source, raw_tail);
            if recorded.is_err() {
                close_keyboard_admission(state);
                return true;
            }
        }
        lease.borrow_mut().deliver_key(outcome, time);
    } else if pressed && outcome.route == keyboard::Route::Local && !raw_tail {
        registry.masks.borrow_mut().local_input(source);
    }
    registry.sync_cursor(state);
    outcome.route == keyboard::Route::Intercept
}
pub fn source_closed(state: &mut State, source: KeyboardSource) {
    let Some(registry) = leases(state) else {
        return;
    };
    let Some(source) = super::global_shortcuts::source_id(source) else {
        disable(state);
        return;
    };
    let participated = registry.masks.borrow().participates(source);
    if participated {
        disable_keyboard(state);
    }
    registry.masks.borrow_mut().remove_source(source);
    let outcomes = registry.keyboard.borrow_mut().source_closed(source);
    if let Some(lease) = registry.active() {
        let time = crate::utils::get_monotonic_time()
            .as_micros()
            .try_into()
            .unwrap_or(u64::MAX);
        for outcome in outcomes {
            lease.borrow_mut().deliver_key(outcome, time);
        }
    }
    registry.sync_cursor(state);
}

fn disable_keyboard(state: &mut State) {
    if let Some(registry) = leases(state) {
        for lease in registry.live() {
            if lease.borrow().grants & 1 != 0 {
                lease.borrow_mut().disable();
            }
        }
        registry.sync_cursor(state);
    }
}
pub fn close_keyboard_admission(state: &mut State) {
    disable_keyboard(state);
    if let Some(registry) = leases(state) {
        registry.keyboard.borrow_mut().close_admission();
    }
}

/// Modifier packets keep their original owner after capture retirement too.
pub fn virtual_modifiers(
    state: &mut State,
    source: KeyboardSource,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) -> Option<keys::Modifiers> {
    refresh(state);
    let input = (depressed, latched, locked, group);
    let Some(registry) = leases(state) else {
        return Some(input);
    };
    let Some(source) = super::global_shortcuts::source_id(source) else {
        close_keyboard_admission(state);
        return Some(input);
    };
    let main = state
        .niri
        .seat
        .get_keyboard()
        .unwrap()
        .modifier_state()
        .serialized;
    let main = (
        main.depressed,
        main.latched,
        main.locked,
        main.layout_effective,
    );
    let lease = registry
        .active()
        .filter(|lease| lease.borrow().grants & 1 != 0);
    let capture = lease.as_ref().and_then(|lease| {
        lease
            .borrow()
            .key_state
            .as_ref()
            .map(keys::State::modifiers)
    });
    let raw_tail = registry.keyboard.borrow().has_suppressed_source(source);
    let result = registry
        .masks
        .borrow_mut()
        .update(source, input, main, capture, raw_tail);
    let output = match result {
        Ok(output) => output,
        Err(_) => {
            close_keyboard_admission(state);
            return Some(if raw_tail { main } else { input });
        }
    };
    if let (Some(lease), Some(captured)) = (lease, output.captured) {
        let mut lease = lease.borrow_mut();
        if let Some(keys) = lease.key_state.as_mut() {
            keys.set_mask(captured.0, captured.1, captured.2, captured.3);
            if let Some(main) = output.ordinary {
                keys.expect_main(main);
            }
        }
        lease.publish_modifiers();
    }
    output.ordinary
}
