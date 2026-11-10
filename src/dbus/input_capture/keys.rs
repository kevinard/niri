//! Capture XKB is independent of the ordinary seat's pressed/modifier state.
use smithay::input::keyboard::xkb;

use super::keyboard::Key;
use crate::niri::State as Niri;

pub type Modifiers = super::masks::Modifiers;
pub struct State {
    state: xkb::State,
    external: Modifiers,
}
fn external(niri: &Niri) -> Modifiers {
    let m = niri
        .niri
        .seat
        .get_keyboard()
        .unwrap()
        .modifier_state()
        .serialized;
    (m.depressed, m.latched, m.locked, m.layout_effective)
}
impl State {
    pub fn new(niri: &mut Niri, local: impl Iterator<Item = u32>, virtual_local: u32) -> Self {
        let keyboard = niri.niri.seat.get_keyboard().unwrap();
        let map = keyboard.with_xkb_state(niri, |context| {
            let xkb = context.xkb().lock().unwrap();
            // SAFETY: only obtain an independently referenced keymap while the
            // ordinary XKB guard lives. Never mutate the ordinary XKB state.
            unsafe { xkb.state() }.get_keymap()
        });
        let mut result = Self {
            state: xkb::State::new(&map),
            external: external(niri),
        };
        for code in local {
            result.update(Key {
                code,
                pressed: true,
            });
        }
        let (depressed, _, _, _) = result.modifiers();
        let (_, latched, locked, group) = result.external;
        result
            .state
            .update_mask(depressed | virtual_local, latched, locked, 0, 0, group);
        result
    }
    pub fn update(&mut self, key: Key) {
        self.state.update_key(
            xkb::Keycode::new(key.code + 8),
            if key.pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );
    }
    pub fn set_mask(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.state
            .update_mask(depressed, latched, locked, 0, 0, group);
    }
    pub fn expect_main(&mut self, masks: Modifiers) {
        // The adapter will apply precisely these filtered ordinary masks. They
        // are not an external change that should overwrite captured mask bits.
        self.external = masks;
    }
    pub fn modifiers(&self) -> Modifiers {
        (
            self.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.state.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.state.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        )
    }
    pub fn pause_combo(&self) -> bool {
        self.state
            .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE)
            && self
                .state
                .mod_name_is_active(xkb::MOD_NAME_ALT, xkb::STATE_MODS_EFFECTIVE)
    }
    pub fn sync_external(&mut self, niri: &Niri) -> bool {
        let next = external(niri);
        let before = self.modifiers();
        if next.1 != self.external.1 || next.2 != self.external.2 || next.3 != self.external.3 {
            self.state.update_mask(
                before.0,
                if next.1 != self.external.1 {
                    next.1
                } else {
                    before.1
                },
                if next.2 != self.external.2 {
                    next.2
                } else {
                    before.2
                },
                0,
                0,
                if next.3 != self.external.3 {
                    next.3
                } else {
                    before.3
                },
            );
        }
        self.external = next;
        before != self.modifiers()
    }
}

pub fn snapshot(niri: &mut Niri) -> zbus::fdo::Result<String> {
    let keyboard = niri.niri.seat.get_keyboard().unwrap();
    let map = keyboard.with_xkb_state(niri, |context| {
        context.xkb().lock().unwrap().keymap_as_string()
    });
    if map.is_empty() || map.len() > 256 * 1024 {
        return Err(zbus::fdo::Error::LimitsExceeded(
            "capture keymap exceeds limit".into(),
        ));
    }
    Ok(map)
}
