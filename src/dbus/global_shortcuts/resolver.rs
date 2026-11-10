//! Resolve the XDG trigger language against a real, borrowed XKB keymap.
//! The parent supplies the same xkb types re-exported by Smithay in production.
use std::collections::BTreeMap;

use super::registry::{Binding, Slot, ALT, CTRL, LOGO, MAX_KEYCODES_PER_BINDING, NUM, SHIFT};
use super::xkb;

const MAX_TRIGGER: usize = 256;
const MAX_CODE_SPAN: u32 = 4096;
const MAX_LAYOUTS: u32 = 32;
const MAX_SYMBOLS: usize = 16_384;
const NAMES: [(u8, &str); 6] = [
    (CTRL, xkb::MOD_NAME_CTRL),
    (ALT, xkb::MOD_NAME_ALT),
    (SHIFT, xkb::MOD_NAME_SHIFT),
    (NUM, xkb::MOD_NAME_NUM),
    (LOGO, xkb::MOD_NAME_LOGO),
    (0, xkb::MOD_NAME_CAPS),
];
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidTrigger,
    UnavailableSymbol,
    MissingModifier,
    KeymapLimit,
    AliasLimit,
}
#[derive(Debug)]
pub struct Trigger {
    symbol: u32,
    modifiers: u8,
}
impl Trigger {
    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.is_empty()
            || value.len() > MAX_TRIGGER
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'+')
        {
            return Err(Error::InvalidTrigger);
        }
        let mut parts = value.rsplit('+');
        let name = parts.next().ok_or(Error::InvalidTrigger)?;
        let mut modifiers = 0;
        for part in parts {
            let bit = match part {
                "CTRL" => CTRL,
                "ALT" => ALT,
                "SHIFT" => SHIFT,
                "NUM" => NUM,
                "LOGO" => LOGO,
                _ => return Err(Error::InvalidTrigger),
            };
            if modifiers & bit != 0 {
                return Err(Error::InvalidTrigger);
            }
            modifiers |= bit;
        }
        let symbol = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS).raw();
        if symbol == 0 {
            return Err(Error::InvalidTrigger);
        }
        // The live seat state already includes a pressed trigger's modifier.
        // Like the compositor matcher, include that implied modifier once.
        modifiers |= match xkb::Keysym::new(symbol) {
            xkb::Keysym::Control_L | xkb::Keysym::Control_R => CTRL,
            xkb::Keysym::Shift_L | xkb::Keysym::Shift_R => SHIFT,
            xkb::Keysym::Alt_L | xkb::Keysym::Alt_R => ALT,
            xkb::Keysym::Super_L | xkb::Keysym::Super_R => LOGO,
            _ => 0,
        };
        Ok(Self { symbol, modifiers })
    }
}
pub struct Snapshot {
    symbols: BTreeMap<u32, Vec<Slot>>,
    base: BTreeMap<Slot, u32>,
    modifiers: Vec<(u8, u32)>,
    recognized_mask: u32,
}
impl Snapshot {
    pub fn new(map: &xkb::Keymap) -> Result<Self, Error> {
        let minimum = map.min_keycode().raw();
        let maximum = map.max_keycode().raw();
        if maximum
            .checked_sub(minimum)
            .is_none_or(|span| span >= MAX_CODE_SPAN)
            || map.num_layouts() > MAX_LAYOUTS
        {
            return Err(Error::KeymapLimit);
        }
        let mut symbols: BTreeMap<u32, Vec<Slot>> = BTreeMap::new();
        let mut base = BTreeMap::new();
        for code in minimum..=maximum {
            let key = xkb::Keycode::new(code);
            let layouts = map.num_layouts_for_key(key);
            if layouts > MAX_LAYOUTS {
                return Err(Error::KeymapLimit);
            }
            for layout in 0..layouts {
                let syms = map.key_get_syms_by_level(key, layout, 0);
                // The ordinary Niri base-level matcher also requires one symbol.
                if syms.len() != 1 || syms[0].raw() == 0 {
                    continue;
                }
                let symbol = syms[0].raw();
                let slot = Slot::new(code, layout);
                base.insert(slot, symbol);
                if !symbols.contains_key(&symbol) && symbols.len() == MAX_SYMBOLS {
                    return Err(Error::KeymapLimit);
                }
                let codes = symbols.entry(symbol).or_default();
                if codes.last() != Some(&slot) {
                    codes.push(slot);
                }
            }
        }
        let mut modifiers = Vec::new();
        let mut recognized_mask = 0;
        for (logical, name) in NAMES {
            let index = map.mod_get_index(name);
            if index < 32 {
                let mask = 1u32 << index;
                recognized_mask |= mask;
                modifiers.push((logical, mask));
            }
        }
        Ok(Self {
            symbols,
            base,
            modifiers,
            recognized_mask,
        })
    }
    pub fn resolve(&self, id: u32, trigger: &str) -> Result<Binding, Error> {
        let parsed = Trigger::parse(trigger)?;
        if id == 0 {
            return Err(Error::InvalidTrigger);
        }
        let available = self
            .modifiers
            .iter()
            .fold(0, |mask, (logical, _)| mask | logical);
        if parsed.modifiers & !available != 0 {
            return Err(Error::MissingModifier);
        }
        let codes = self
            .symbols
            .get(&parsed.symbol)
            .ok_or(Error::UnavailableSymbol)?;
        if codes.len() > MAX_KEYCODES_PER_BINDING {
            return Err(Error::AliasLimit);
        }
        Ok(Binding {
            id,
            keys: codes.clone(),
            modifiers: parsed.modifiers,
        })
    }
    /// Match the current base layout first. Like ordinary Niri binds, only a
    /// non-ASCII base key may fall back to the first ASCII layout for that key.
    pub fn candidates(&self, code: u32, layout: u32) -> Vec<Slot> {
        let current = Slot::new(code, layout);
        let mut slots = vec![current];
        let ascii = |symbol: u32| (0x21..=0x7e).contains(&symbol);
        if !self.base.get(&current).is_some_and(|&symbol| ascii(symbol)) {
            if let Some((&slot, _)) = self
                .base
                .range(Slot::new(code, 0)..=Slot::new(code, u32::MAX))
                .find(|(_, symbol)| ascii(**symbol))
            {
                if slot != current {
                    slots.push(slot);
                }
            }
        }
        slots
    }
    /// Serialized XKB effective state uses the current keymap's modifier indexes.
    /// Caps Lock does not change a base-key trigger. Unrecognized bits are not
    /// silently accepted as an ordinary combination by the registry matcher.
    pub fn logical_modifiers(&self, effective: u32) -> u8 {
        let mut result = if effective & !self.recognized_mask != 0 {
            128
        } else {
            0
        };
        for (logical, mask) in &self.modifiers {
            if effective & mask != 0 {
                result |= logical;
            }
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::super::registry::Registry;
    use super::*;
    fn map(layout: &str) -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap()
    }
    #[test]
    fn modifier_trigger_matches_the_real_post_press_xkb_state() {
        let map = map("us");
        let snapshot = Snapshot::new(&map).unwrap();
        let binding = snapshot.resolve(7, "Control_L").unwrap();
        let key = binding.keys[0];
        let mut live = xkb::State::new(&map);
        live.update_key(xkb::Keycode::new(key.code), xkb::KeyDirection::Down);
        let modifiers = snapshot.logical_modifiers(live.serialize_mods(xkb::STATE_MODS_EFFECTIVE));
        let mut registry = Registry::default();
        let lease = registry.open().unwrap();
        registry
            .replace(lease, vec![binding], |_, _| false)
            .unwrap();
        assert_eq!(registry.lookup(key, modifiers).unwrap().binding, 7);
    }
    #[test]
    fn real_us_and_french_keymaps_resolve_the_same_logical_shortcut_differently() {
        let us = Snapshot::new(&map("us")).unwrap();
        let fr = Snapshot::new(&map("fr")).unwrap();
        assert_eq!(
            us.resolve(1, "CTRL+a").unwrap().keys,
            vec![Slot::new(38, 0)]
        );
        assert_eq!(
            fr.resolve(1, "CTRL+a").unwrap().keys,
            vec![Slot::new(24, 0)]
        );
        let both = Snapshot::new(&map("fr,us")).unwrap();
        assert_eq!(
            both.resolve(1, "CTRL+a").unwrap().keys,
            vec![Slot::new(24, 0), Slot::new(38, 1)]
        );
    }
    #[test]
    fn duplicate_layout_aliases_do_not_create_duplicate_physical_registrations() {
        let snapshot = Snapshot::new(&map("us,us")).unwrap();
        assert_eq!(
            snapshot.resolve(1, "CTRL+a").unwrap().keys,
            vec![Slot::new(38, 0), Slot::new(38, 1)]
        );
    }
    #[test]
    fn shifted_triggers_use_the_requested_base_key_and_keep_the_shift_requirement() {
        let snapshot = Snapshot::new(&map("us")).unwrap();
        let binding = snapshot.resolve(1, "CTRL+SHIFT+a").unwrap();
        assert_eq!(binding.keys, vec![Slot::new(38, 0)]);
        assert_eq!(binding.modifiers, CTRL | SHIFT);
        assert!(matches!(
            snapshot.resolve(1, "CTRL+A"),
            Err(Error::UnavailableSymbol)
        ));
    }
    #[test]
    fn invalid_or_ambiguous_trigger_text_is_not_silently_reinterpreted() {
        for value in [
            "",
            "CTRL+",
            "CTRL++a",
            "CTRL+CTRL+a",
            "SUPER+a",
            "CTRL+!",
            "ctrl+a",
            "a suffix",
            "CTRL+does_not_exist",
            "CTRL+ä",
        ] {
            assert_eq!(
                Trigger::parse(value).unwrap_err(),
                Error::InvalidTrigger,
                "{value}"
            );
        }
        assert_eq!(
            Trigger::parse(&"a".repeat(257)).unwrap_err(),
            Error::InvalidTrigger
        );
    }
    #[test]
    fn actual_named_modifier_state_matches_without_assuming_numeric_indexes() {
        let map = map("us");
        let snapshot = Snapshot::new(&map).unwrap();
        let mut state = xkb::State::new(&map);
        state.update_key(xkb::Keycode::new(37), xkb::KeyDirection::Down);
        assert_eq!(
            snapshot.logical_modifiers(state.serialize_mods(xkb::STATE_MODS_EFFECTIVE)),
            CTRL
        );
        state.update_key(xkb::Keycode::new(50), xkb::KeyDirection::Down);
        assert_eq!(
            snapshot.logical_modifiers(state.serialize_mods(xkb::STATE_MODS_EFFECTIVE)),
            CTRL | SHIFT
        );
        state.update_mask(0, 0, 1 << map.mod_get_index(xkb::MOD_NAME_CAPS), 0, 0, 0);
        assert_eq!(
            snapshot.logical_modifiers(state.serialize_mods(xkb::STATE_MODS_EFFECTIVE)),
            0
        );
        state.update_mask(0, 0, 1 << map.mod_get_index(xkb::MOD_NAME_NUM), 0, 0, 0);
        assert_eq!(
            snapshot.logical_modifiers(state.serialize_mods(xkb::STATE_MODS_EFFECTIVE)),
            NUM
        );
        assert_eq!(
            snapshot.logical_modifiers(1 << map.mod_get_index("Mod5")),
            128
        );
    }
    #[test]
    fn different_layouts_can_bind_a_and_q_without_a_false_physical_conflict() {
        let snapshot = Snapshot::new(&map("fr,us")).unwrap();
        let mut registry = Registry::default();
        let lease = registry.open().unwrap();
        registry
            .replace(
                lease,
                vec![
                    snapshot.resolve(1, "CTRL+a").unwrap(),
                    snapshot.resolve(2, "CTRL+q").unwrap(),
                ],
                |_, _| false,
            )
            .unwrap();
        assert_eq!(registry.lookup(Slot::new(24, 0), CTRL).unwrap().binding, 1);
        assert_eq!(registry.lookup(Slot::new(24, 1), CTRL).unwrap().binding, 2);
        assert_eq!(registry.lookup(Slot::new(38, 0), CTRL).unwrap().binding, 2);
        assert_eq!(registry.lookup(Slot::new(38, 1), CTRL).unwrap().binding, 1);
    }
    #[test]
    fn current_ascii_layout_does_not_accidentally_match_another_layouts_key() {
        let snapshot = Snapshot::new(&map("fr,us")).unwrap();
        assert_eq!(snapshot.candidates(24, 0), vec![Slot::new(24, 0)]);
        assert_eq!(snapshot.candidates(24, 1), vec![Slot::new(24, 1)]);
    }
    #[test]
    fn non_ascii_layout_tries_the_current_key_before_the_first_ascii_fallback() {
        let snapshot = Snapshot::new(&map("ru,us")).unwrap();
        assert_eq!(
            snapshot.candidates(38, 0),
            vec![Slot::new(38, 0), Slot::new(38, 1)]
        );
        assert_eq!(snapshot.candidates(38, 1), vec![Slot::new(38, 1)]);
    }
    #[test]
    fn resolved_layout_alias_conflicts_are_checked_by_the_real_registry() {
        let snapshot = Snapshot::new(&map("us")).unwrap();
        let mut registry = Registry::default();
        let a = registry.open().unwrap();
        let b = registry.open().unwrap();
        registry
            .replace(
                a,
                vec![snapshot.resolve(1, "CTRL+Prior").unwrap()],
                |_, _| false,
            )
            .unwrap();
        assert!(registry
            .replace(
                b,
                vec![snapshot.resolve(2, "CTRL+Page_Up").unwrap()],
                |_, _| { false }
            )
            .is_err());
    }
}
