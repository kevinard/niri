//! Private compositor shortcut leases. This is input authority for a trusted
//! desktop backend, not XDG consent or an application-facing portal interface.
use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use niri_config::{Bind, Config, ModKey, Modifiers};
use smithay::backend::input::Device;
pub use smithay::input::keyboard::xkb;
use smithay::input::keyboard::{KeyboardSource, KeymapFileId, Keysym};
use smithay::wayland::virtual_keyboard::VirtualKeyboardDevice;
use zbus::{fdo, interface, zvariant};

use super::freedesktop_a11y::KeyboardMonitor;
use super::Start;
use crate::input::remote_keyboard::RemoteKeyboardDevice;
use crate::niri::State;
use crate::utils::get_monotonic_time;

mod engine;
mod host;
mod presses;
mod registry;
mod resolver;
mod state;
mod wire;

type Reply = async_channel::Sender<fdo::Result<()>>;
pub struct GlobalShortcuts {
    to_niri: calloop::channel::SyncSender<Open>,
    admission: Arc<AtomicUsize>,
}
struct Permit(Arc<AtomicUsize>);
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
impl GlobalShortcuts {
    pub fn new(to_niri: calloop::channel::SyncSender<Open>) -> Self {
        Self {
            to_niri,
            admission: Arc::new(AtomicUsize::new(0)),
        }
    }
}
#[interface(name = "org.canopa.Niri.GlobalShortcuts1")]
impl GlobalShortcuts {
    async fn open_shortcuts(&self) -> fdo::Result<zvariant::OwnedFd> {
        self.admission
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < registry::MAX_LEASES).then_some(n + 1)
            })
            .map_err(|_| fdo::Error::LimitsExceeded("too many shortcut leases".into()))?;
        let permit = Permit(self.admission.clone());
        let (client, stream) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        let (reply, receive) = async_channel::bounded(1);
        self.to_niri
            .try_send(Open {
                stream,
                permit,
                reply,
            })
            .map_err(|_| fdo::Error::LimitsExceeded("shortcut admission unavailable".into()))?;
        receive
            .recv()
            .await
            .map_err(|_| fdo::Error::Failed("compositor stopped".into()))??;
        Ok(zvariant::OwnedFd::from(OwnedFd::from(client)))
    }
    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
}
impl Start for GlobalShortcuts {
    fn start(self, _monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        Ok(zbus::blocking::connection::Builder::session()?
            .name("org.canopa.Niri.GlobalShortcuts1")?
            .serve_at("/org/canopa/Niri/GlobalShortcuts1", self)?
            .build()?)
    }
}
#[derive(PartialEq)]
struct Cache {
    map: KeymapFileId,
    binds: Vec<Bind>,
    mru: Vec<Bind>,
    mru_on: bool,
    power_disabled: bool,
    mod_key: ModKey,
    a11y: u64,
}
impl Cache {
    fn matches(&self, map: KeymapFileId, config: &Config, mod_key: ModKey, a11y: u64) -> bool {
        self.map == map
            && self.binds == config.binds.0
            && self.mru == config.recent_windows.binds
            && self.mru_on == config.recent_windows.on
            && self.power_disabled == config.input.disable_power_key_handling
            && self.mod_key == mod_key
            && self.a11y == a11y
    }
}
#[derive(Default)]
struct Inner {
    host: host::Host,
    cache: Option<Cache>,
    suspended: bool,
}
#[derive(Clone, Default)]
pub struct Leases {
    inner: Rc<RefCell<Inner>>,
    physical: Rc<RefCell<HashMap<(TypeId, String), KeyboardSource>>>,
    exhausted: Rc<Cell<bool>>,
}
fn leases(state: &State) -> Option<Leases> {
    state
        .niri
        .dbus
        .as_ref()
        .map(|d| d.global_shortcut_leases.clone())
}
fn now() -> u64 {
    get_monotonic_time()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn source_id(source: KeyboardSource) -> Option<u64> {
    match source {
        KeyboardSource::Physical => Some(1),
        KeyboardSource::Auxiliary(id) if id != 0 => id.checked_add(1),
        _ => None,
    }
}
fn admission_unavailable(state: &State) -> bool {
    state.niri.is_locked()
        || state.niri.screenshot_ui.is_open()
        || state.niri.exit_confirm_dialog.is_open()
        || state.niri.window_mru_ui.is_open()
        || state.niri.keyboard_focus.is_overview()
}
fn unavailable(state: &State) -> bool {
    admission_unavailable(state) || state.is_inhibiting_shortcuts()
}
/// A probe state shares the borrowed keymap, never the live seat state.
fn reserved(
    map: &xkb::Keymap,
    probe: &mut xkb::State,
    config: &Config,
    mod_key: ModKey,
    monitor: Option<&KeyboardMonitor>,
    slot: registry::Slot,
    flags: u8,
) -> bool {
    use registry::{ALT, CTRL, LOGO, NUM, SHIFT};
    let mut effective = 0;
    let mask = |name| {
        let index = map.mod_get_index(name);
        if index < 32 {
            1u32 << index
        } else {
            0
        }
    };
    let mut mods = Modifiers::empty();
    for (bit, name, normal) in [
        (CTRL, xkb::MOD_NAME_CTRL, Modifiers::CTRL),
        (ALT, xkb::MOD_NAME_ALT, Modifiers::ALT),
        (SHIFT, xkb::MOD_NAME_SHIFT, Modifiers::SHIFT),
        (LOGO, xkb::MOD_NAME_LOGO, Modifiers::SUPER),
        (NUM, xkb::MOD_NAME_NUM, Modifiers::empty()),
    ] {
        if flags & bit != 0 {
            effective |= mask(name);
            mods |= normal;
        }
    }
    let code = xkb::Keycode::new(slot.code);
    let base = map.key_get_syms_by_level(code, slot.layout, 0);
    let mut raw = base.to_vec();
    let ascii = |sym: &Keysym| sym.key_char().is_some_and(|c| c.is_ascii_graphic());
    if base.len() != 1 || !ascii(&base[0]) {
        for layout in 0..map.num_layouts_for_key(code) {
            let syms = map.key_get_syms_by_level(code, layout, 0);
            if syms.len() == 1 && ascii(&syms[0]) {
                if !raw.contains(&syms[0]) {
                    raw.push(syms[0]);
                }
                break;
            }
        }
    }
    // Ordinary triggers ignore lock state, so reserve conflicts in either Caps
    // state and either Num state unless NUM was explicitly requested.
    for num in [0, mask(xkb::MOD_NAME_NUM)] {
        if flags & NUM != 0 && num == 0 {
            continue;
        }
        for caps in [0, mask(xkb::MOD_NAME_CAPS)] {
            probe.update_mask(effective | num | caps, 0, 0, 0, 0, slot.layout);
            let sym = probe.key_get_one_sym(code);
            if crate::input::portal_shortcut_reserved(config, mod_key, sym, &raw, mods)
                || monitor.is_some_and(|m| m.shortcut_reserved(sym, effective | num | caps, mods))
            {
                return true;
            }
        }
    }
    false
}
fn with_reservations<R>(
    state: &mut State,
    operation: impl FnOnce(&xkb::Keymap, &Config, ModKey, Option<&KeyboardMonitor>) -> R,
) -> R {
    let keyboard = state.niri.seat.get_keyboard().unwrap();
    let config = state.niri.config.clone();
    let mod_key = state.backend.mod_key(&config.borrow());
    let monitor = state
        .niri
        .a11y_manager
        .as_ref()
        .map(|m| m.keyboard_monitor.clone());
    let mut operation = Some(operation);
    keyboard.with_xkb_state(state, |context| {
        let xkb = context.xkb().lock().unwrap();
        // SAFETY: keymap is borrowed within the XKB guard and cannot escape.
        let map = unsafe { xkb.keymap() };
        operation.take().unwrap()(&map, &config.borrow(), mod_key, monitor.as_ref())
    })
}
pub fn refresh(state: &mut State) {
    let Some(leases) = leases(state) else {
        return;
    };
    if leases.exhausted.get() {
        return;
    }
    let unavailable = unavailable(state);
    let time = now();
    {
        let mut inner = leases.inner.borrow_mut();
        if unavailable && !inner.suspended {
            inner.host.suspend(time);
        }
        inner.suspended = unavailable;
        if inner.host.is_empty() {
            return;
        }
    }
    let map_id = state.niri.seat.get_keyboard().unwrap().keymap_id();
    let a11y = state
        .niri
        .a11y_manager
        .as_ref()
        .map_or(0, |m| m.keyboard_monitor.shortcut_revision());
    let changed = {
        let config = state.niri.config.borrow();
        let mod_key = state.backend.mod_key(&config);
        !leases
            .inner
            .borrow()
            .cache
            .as_ref()
            .is_some_and(|c| c.matches(map_id, &config, mod_key, a11y))
    };
    if !changed {
        return;
    }
    with_reservations(state, |map, config, mod_key, monitor| {
        let snapshot = if a11y == u64::MAX {
            None
        } else {
            resolver::Snapshot::new(map).ok()
        };
        let mut probe = xkb::State::new(map);
        let mut inner = leases.inner.borrow_mut();
        inner.host.revalidate(
            snapshot,
            |slot, flags| reserved(map, &mut probe, config, mod_key, monitor, slot, flags),
            time,
        );
        inner.cache = Some(Cache {
            map: map_id,
            binds: config.binds.0.clone(),
            mru: config.recent_windows.binds.clone(),
            mru_on: config.recent_windows.on,
            power_disabled: config.input.disable_power_key_handling,
            mod_key,
            a11y,
        });
    });
}
pub fn open(state: &mut State, message: Open, leases: &Leases) {
    // An inhibitor suspends activation, not registration ownership. Permit an
    // empty trusted-backend lease so another consent editor can still configure
    // shortcuts without waiting for the first editor to lose focus.
    if leases.exhausted.get() || admission_unavailable(state) {
        let _ = message.reply.try_send(Err(fdo::Error::AccessDenied(
            "shortcuts unavailable".into(),
        )));
        return;
    }
    let registration = match leases.inner.borrow_mut().host.open(message.stream) {
        Ok(r) => r,
        Err(error) => {
            let _ = message
                .reply
                .try_send(Err(fdo::Error::Failed(error.to_string())));
            return;
        }
    };
    refresh(state);
    let id = registration.id;
    let owner = leases.clone();
    let permit = message.permit;
    let result = state.niri.event_loop.insert_source(
        Generic::new(registration.stream, Interest::READ, Mode::Level),
        move |_, _, state| {
            let _permit = &permit;
            refresh(state);
            let keep = with_reservations(state, |map, config, mod_key, monitor| {
                let mut probe = xkb::State::new(map);
                owner.inner.borrow_mut().host.dispatch(
                    id,
                    |slot, flags| reserved(map, &mut probe, config, mod_key, monitor, slot, flags),
                    now(),
                )
            });
            Ok(if keep {
                PostAction::Continue
            } else {
                PostAction::Remove
            })
        },
    );
    if result.is_err() {
        leases.inner.borrow_mut().host.close(id, now());
    }
    let _ = message.reply.try_send(
        result
            .map(|_| ())
            .map_err(|e| fdo::Error::Failed(e.to_string())),
    );
}
/// Distinct physical sources preserve overlapping holds, as virtual/remote
/// sources already do. An exhausted bounded identity table revokes shortcut
/// authority and refuses further admission until compositor restart.
pub fn physical_source<D: Device + 'static>(state: &State, device: &D) -> Option<KeyboardSource> {
    let leases = leases(state)?;
    let key = (TypeId::of::<D>(), device.id());
    if let Some(source) = leases.physical.borrow().get(&key) {
        return Some(*source);
    }
    if leases.physical.borrow().len() >= 256 {
        leases.exhausted.set(true);
        leases.inner.borrow_mut().host.close_all(now());
        return None;
    }
    let source = KeyboardSource::new_auxiliary();
    if source_id(source).is_none() {
        leases.exhausted.set(true);
        leases.inner.borrow_mut().host.close_all(now());
        return None;
    }
    leases.physical.borrow_mut().insert(key, source);
    Some(source)
}
pub fn raw_edge(
    state: &mut State,
    source: KeyboardSource,
    code: u32,
    pressed: bool,
    time: u64,
) -> bool {
    refresh(state);
    let Some(leases) = leases(state) else {
        return false;
    };
    let Some(source) = source_id(source) else {
        leases.inner.borrow_mut().host.close_all(time);
        return false;
    };
    let captured = leases
        .inner
        .borrow_mut()
        .host
        .raw_edge(source, code, pressed, time);
    captured
}
pub fn accepted_press(
    state: &mut State,
    source: KeyboardSource,
    code: u32,
    layout: u32,
    modifiers: u32,
    time: u64,
) -> bool {
    if unavailable(state) {
        return false;
    }
    let Some(leases) = leases(state) else {
        return false;
    };
    if leases.exhausted.get() {
        return false;
    }
    let Some(source) = source_id(source) else {
        return false;
    };
    let captured = leases
        .inner
        .borrow_mut()
        .host
        .accepted_press(source, code, layout, modifiers, time);
    captured
}
pub fn seat_key(state: &State, code: u32, time: u64) {
    if let Some(leases) = leases(state) {
        let down = state
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .contains(&xkb::Keycode::new(code));
        leases.inner.borrow_mut().host.seat_key(code, down, time);
    }
}
pub fn source_closed(state: &State, source: KeyboardSource) {
    if let (Some(leases), Some(source)) = (leases(state), source_id(source)) {
        let down = state
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .iter()
            .map(|k| k.raw())
            .collect::<BTreeSet<_>>();
        leases
            .inner
            .borrow_mut()
            .host
            .source_closed(source, &down, now());
    }
}
pub fn remove_device<D: Device + 'static>(state: &mut State, device: &D) {
    let Some(leases) = leases(state) else {
        return;
    };
    let source = if let Some(virtual_device) =
        (device as &dyn Any).downcast_ref::<VirtualKeyboardDevice>()
    {
        Some(virtual_device.source())
    } else if let Some(remote) = (device as &dyn Any).downcast_ref::<RemoteKeyboardDevice>() {
        Some(remote.source)
    } else {
        leases
            .physical
            .borrow_mut()
            .remove(&(TypeId::of::<D>(), device.id()))
    };
    if let Some(source) = source {
        let keyboard = state.niri.seat.get_keyboard().unwrap();
        let before = keyboard.pressed_keys();
        keyboard.release_source(state, source);
        let down = keyboard.pressed_keys();
        let released: Vec<_> = before.difference(&down).copied().collect();
        for key in &released {
            state.niri.pending_release_binds.remove(key);
            state.niri.suppressed_keys.remove(key);
        }
        if !released.is_empty() {
            if let Some(token) = state.niri.bind_repeat_timer.take() {
                state.niri.event_loop.remove(token);
            }
        }
        source_closed(state, source);
    }
}
pub fn reset(state: &State) {
    if let Some(leases) = leases(state) {
        let mut inner = leases.inner.borrow_mut();
        let time = now();
        inner.host.suspend(time);
        let down = state
            .niri
            .seat
            .get_keyboard()
            .unwrap()
            .pressed_keys()
            .iter()
            .map(|k| k.raw())
            .collect();
        inner.host.reconcile(&down, time);
    }
}
