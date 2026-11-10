//! Private compositor capture lease. Consent and EI translation belong to the
//! portal; this FD is a control capability and must never be handed to an app.
//! Capture version 1 diverts pointer motion/buttons/axis only. It does not claim
//! keyboard/touch capture or implement the public InputCapture interface.
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
use zbus::{fdo, interface, zvariant};

use super::Start;
use crate::niri::{PointerVisibility, State};

mod geometry;
use geometry::{Barrier, Space, Zone};

const MAX_LEASES: usize = 8;
const MAX_MESSAGE: usize = 16 * 1024;
type Zones = Vec<(u32, u32, i32, i32)>;
type Reply = async_channel::Sender<fdo::Result<(u32, Zones)>>;

pub struct InputCapture {
    to_niri: calloop::channel::SyncSender<Open>,
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
}
impl InputCapture {
    pub fn new(to_niri: calloop::channel::SyncSender<Open>) -> Self {
        Self {
            to_niri,
            admission: Arc::new(AtomicUsize::new(0)),
        }
    }
}
#[interface(name = "org.canopa.Niri.InputCapture1")]
impl InputCapture {
    async fn open_pointer(&self) -> fdo::Result<(zvariant::OwnedFd, u32, Zones)> {
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
            .try_send(Open {
                stream,
                permit,
                reply,
            })
            .map_err(|_| fdo::Error::LimitsExceeded("capture admission unavailable".into()))?;
        let (generation, zones) = receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok((
            zvariant::OwnedFd::from(OwnedFd::from(client)),
            generation,
            zones,
        ))
    }
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
    #[zbus(property)]
    fn supported_capabilities(&self) -> u32 {
        2
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
        if self.active().is_some() {
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
    Enable,
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
    fn retire(&mut self) {
        self.enabled = false;
        self.active = None;
        self.retired = true;
        let _ = self.writer.shutdown(Shutdown::Both);
    }
    fn feedback(&mut self, event: Event) -> bool {
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
            self.feedback(Event::Deactivated { activation });
        }
    }
    fn disable(&mut self) {
        self.deactivate();
        if std::mem::take(&mut self.enabled) {
            self.feedback(Event::Disabled);
        }
    }
    fn command(&mut self, request: Request, busy: bool, available: bool) {
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
            Command::Enable => {
                if !available {
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

pub fn open(state: &mut State, message: Open, entries: &Leases) {
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
    let initial = (space.generation(), describe(&space));
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
                lease
                    .borrow_mut()
                    .command(request, busy, !unavailable(state));
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
    for lease in registry.live() {
        let mut lease = lease.borrow_mut();
        if lease.retired {
            continue;
        }
        if unavailable(state) {
            lease.disable();
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
        lease.activation = lease.activation.wrapping_add(1).max(1);
        let activation = lease.activation;
        lease.active = Some(activation);
        lease.position = to;
        lease.feedback(Event::Activated {
            activation,
            barrier,
            position: to,
        })
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
