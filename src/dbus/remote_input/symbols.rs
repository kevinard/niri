//! Source-owned logical keyboard transitions using a borrowed authoritative keymap.
//! A resolved press keeps its exact chord until release; no global keymap writes.
use std::collections::HashMap;

use smithay::input::keyboard::xkb;

const MAX_KEYS: usize = 64;
const MAX_CODE: u32 = 0x2ff;
type Transition = (u32, bool);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Code(u32),
    Symbol(u32),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Unavailable,
    Limit,
    Conflict,
}

#[derive(Default)]
pub struct Planner {
    held: HashMap<Key, Vec<u32>>,
    references: HashMap<u32, usize>,
}
impl Planner {
    fn state(map: &xkb::Keymap, mods: [u32; 4], additional: u32) -> xkb::State {
        let mut state = xkb::State::new(map);
        state.update_mask(mods[0] | additional, mods[1], mods[2], 0, 0, mods[3]);
        state
    }
    fn resolve(&self, map: &xkb::Keymap, symbol: u32, mods: [u32; 4]) -> Result<Vec<u32>, Error> {
        if symbol == 0 || symbol > 0x1fff_ffff {
            return Err(Error::Invalid);
        }
        let state = Self::state(map, mods, 0);
        let layout = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        if layout >= map.num_layouts() {
            return Err(Error::Unavailable);
        }
        let mut candidates = Vec::new();
        let mut selectors = HashMap::new();
        // Only momentary level selectors. Never toggle Caps/Num Lock to type a symbol.
        for modifier in [
            xkb::keysyms::KEY_Shift_L,
            xkb::keysyms::KEY_Shift_R,
            xkb::keysyms::KEY_ISO_Level3_Shift,
            xkb::keysyms::KEY_ISO_Level5_Shift,
            xkb::keysyms::KEY_Mode_switch,
        ] {
            for code in 1..=MAX_CODE {
                let key = xkb::Keycode::new(code + 8);
                if map
                    .key_get_syms_by_level(key, layout, 0)
                    .iter()
                    .any(|sym| sym.raw() == modifier)
                {
                    let mut isolated = Self::state(map, [0, 0, 0, layout], 0);
                    isolated.update_key(key, xkb::KeyDirection::Down);
                    let mask = isolated.serialize_mods(xkb::STATE_MODS_DEPRESSED);
                    if mask != 0 && !candidates.iter().any(|&(_, existing)| existing == mask) {
                        candidates.push((code, mask));
                    }
                    if mask != 0 {
                        selectors.insert(code, mask);
                    }
                }
            }
        }
        let selector_mask = candidates
            .iter()
            .fold(0, |mask, &(_, modifier)| mask | modifier);
        let mut actual_mods = mods;
        for code in self.references.keys() {
            actual_mods[0] |= selectors.get(code).copied().unwrap_or_default();
        }
        // Resolve an independent chord. Feedback may already include another
        // held symbol's Shift/AltGr, but that symbol can be released first.
        let mut canonical_mods = actual_mods;
        canonical_mods[0] &= !selector_mask;
        canonical_mods[1] &= !selector_mask;
        let mut subsets: Vec<_> = (0usize..1 << candidates.len()).collect();
        subsets.sort_by_key(|mask| mask.count_ones());
        for subset in subsets {
            let mut chord = Vec::new();
            let mut mask = 0;
            for (index, &(code, modifier)) in candidates.iter().enumerate() {
                if subset & (1 << index) != 0 {
                    chord.push(code);
                    mask |= modifier;
                }
            }
            let state = Self::state(map, canonical_mods, mask);
            let actual = Self::state(map, actual_mods, mask);
            for code in 1..=MAX_CODE {
                let key = xkb::Keycode::new(code + 8);
                let syms = state.key_get_syms(key);
                let real = actual.key_get_syms(key);
                if syms.len() == 1
                    && syms[0].raw() == symbol
                    && real.len() == 1
                    && real[0].raw() == symbol
                    && !chord.contains(&code)
                {
                    chord.push(code);
                    return Ok(chord);
                }
            }
        }
        Err(Error::Unavailable)
    }
    pub fn press(
        &mut self,
        map: &xkb::Keymap,
        key: Key,
        mods: [u32; 4],
    ) -> Result<Vec<Transition>, Error> {
        if self.held.contains_key(&key) {
            return Err(Error::Invalid);
        }
        if self.held.len() >= MAX_KEYS {
            return Err(Error::Limit);
        }
        let chord = match key {
            Key::Code(code) if (1..=MAX_CODE).contains(&code) => vec![code],
            Key::Code(_) => return Err(Error::Invalid),
            Key::Symbol(symbol) => self.resolve(map, symbol, mods)?,
        };
        if let Key::Symbol(_) = key {
            if self.held.iter().any(|(other, existing)| {
                matches!(other, Key::Symbol(_)) && existing.last() == chord.last()
            }) {
                // One physical key cannot send a second symbol's press while
                // its first symbol remains held. Do not change modifiers alone.
                return Err(Error::Conflict);
            }
        }
        let extra = chord
            .iter()
            .filter(|code| !self.references.contains_key(code))
            .count();
        if self.references.len() + extra > MAX_KEYS {
            return Err(Error::Limit);
        }
        let mut events = Vec::new();
        for &code in &chord {
            let references = self.references.entry(code).or_default();
            if *references == 0 {
                events.push((code, true));
            }
            *references += 1;
        }
        self.held.insert(key, chord);
        Ok(events)
    }
    pub fn release(&mut self, key: Key) -> Result<Vec<Transition>, Error> {
        let chord = self.held.remove(&key).ok_or(Error::Invalid)?;
        let mut events = Vec::new();
        for code in chord.into_iter().rev() {
            let references = self
                .references
                .get_mut(&code)
                .expect("held chord reference");
            *references -= 1;
            if *references == 0 {
                self.references.remove(&code);
                events.push((code, false));
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        planner: Planner,
        map: xkb::Keymap,
    }
    impl std::ops::Deref for Fixture {
        type Target = Planner;
        fn deref(&self) -> &Planner {
            &self.planner
        }
    }
    impl Fixture {
        fn press(&mut self, key: Key, mods: [u32; 4]) -> Result<Vec<Transition>, Error> {
            self.planner.press(&self.map, key, mods)
        }
        fn release(&mut self, key: Key) -> Result<Vec<Transition>, Error> {
            self.planner.release(key)
        }
        fn state(&self, mods: [u32; 4], additional: u32) -> xkb::State {
            Planner::state(&self.map, mods, additional)
        }
    }
    fn fixture(layout: &str) -> Fixture {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let map =
            xkb::Keymap::new_from_names(&context, "evdev", "pc105", layout, "", None, 0).unwrap();
        Fixture {
            planner: Planner::default(),
            map,
        }
    }
    fn symbol(value: u32) -> Key {
        Key::Symbol(value)
    }
    #[test]
    fn french_and_us_symbols_follow_the_configured_layout() {
        let mut fr = fixture("fr");
        assert_eq!(fr.press(symbol(b'a' as u32), [0; 4]), Ok(vec![(16, true)]));
        assert_eq!(fr.release(symbol(b'a' as u32)), Ok(vec![(16, false)]));
        assert_eq!(
            fr.press(symbol(b'A' as u32), [0; 4]),
            Ok(vec![(42, true), (16, true)])
        );
        assert_eq!(
            fr.release(symbol(b'A' as u32)),
            Ok(vec![(16, false), (42, false)])
        );
        let mut us = fixture("us");
        assert_eq!(us.press(symbol(b'a' as u32), [0; 4]), Ok(vec![(30, true)]));
    }
    #[test]
    fn chords_hold_shared_modifiers_and_mixed_raw_keys_until_the_last_owner() {
        let mut p = fixture("fr");
        assert_eq!(
            p.press(symbol(b'A' as u32), [0; 4]),
            Ok(vec![(42, true), (16, true)])
        );
        // Intentionally use zero feedback: the planner still owns its earlier Shift.
        assert_eq!(p.press(symbol(b'B' as u32), [0; 4]), Ok(vec![(48, true)]));
        assert_eq!(p.press(Key::Code(42), [0; 4]), Ok(vec![]));
        assert_eq!(p.release(symbol(b'A' as u32)), Ok(vec![(16, false)]));
        assert_eq!(p.release(symbol(b'B' as u32)), Ok(vec![(48, false)]));
        assert_eq!(p.release(Key::Code(42)), Ok(vec![(42, false)]));
    }
    #[test]
    fn real_modifier_feedback_does_not_make_a_second_symbol_borrow_the_first_chord() {
        let mut p = fixture("fr");
        p.press(symbol(b'A' as u32), [0; 4]).unwrap();
        let shift = 1 << p.map.mod_get_index(&xkb::MOD_NAME_SHIFT);
        assert_eq!(
            p.press(symbol(b'B' as u32), [shift, 0, 0, 0]),
            Ok(vec![(48, true)])
        );
        assert_eq!(p.release(symbol(b'A' as u32)), Ok(vec![(16, false)]));
        assert_eq!(
            p.release(symbol(b'B' as u32)),
            Ok(vec![(48, false), (42, false)])
        );
    }
    #[test]
    fn external_shift_is_preserved_and_conflicting_symbols_are_not_substituted() {
        let mut p = fixture("fr");
        let shift = 1 << p.map.mod_get_index(&xkb::MOD_NAME_SHIFT);
        assert_eq!(
            p.press(symbol(b'A' as u32), [shift, 0, 0, 0]),
            Ok(vec![(42, true), (16, true)])
        );
        assert_eq!(
            p.release(symbol(b'A' as u32)),
            Ok(vec![(16, false), (42, false)])
        );
        assert_eq!(
            p.press(symbol(b'a' as u32), [shift, 0, 0, 0]),
            Err(Error::Unavailable)
        );
        p.press(Key::Code(54), [0; 4]).unwrap();
        assert_eq!(
            p.press(symbol(b'a' as u32), [0; 4]),
            Err(Error::Unavailable)
        );
        assert_eq!(p.release(Key::Code(54)), Ok(vec![(54, false)]));
    }
    #[test]
    fn french_altgr_symbol_retains_its_level_selector() {
        let mut p = fixture("fr");
        let at = symbol(xkb::keysyms::KEY_at);
        let events = p.press(at, [0; 4]).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1], (11, true));
        // XKB may provide an internal level-selector key before physical RAlt.
        // Check the actual resulting symbol instead of assuming a modifier code.
        let mut state = p.state([0; 4], 0);
        for &(code, pressed) in &events {
            assert!(pressed);
            state.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Down);
        }
        assert_eq!(
            state.key_get_one_sym(xkb::Keycode::new(19)).raw(),
            xkb::keysyms::KEY_at
        );
        assert_eq!(p.release(at), Ok(vec![(11, false), (events[0].0, false)]));
    }
    #[test]
    fn different_symbols_cannot_reuse_a_held_base_key_and_change_only_modifiers() {
        let mut p = fixture("fr");
        let lower = symbol(b'a' as u32);
        let upper = symbol(b'A' as u32);
        assert_eq!(p.press(lower, [0; 4]), Ok(vec![(16, true)]));
        assert_eq!(p.press(upper, [0; 4]), Err(Error::Conflict));
        assert_eq!(p.release(upper), Err(Error::Invalid));
        assert_eq!(p.release(lower), Ok(vec![(16, false)]));
        assert!(p.references.is_empty());
    }
    #[test]
    fn ascii_chords_produce_requested_symbols_and_preserve_caps_and_group() {
        for layout in ["us", "fr", "us,fr"] {
            let mut p = fixture(layout);
            let caps = 1 << p.map.mod_get_index(&xkb::MOD_NAME_CAPS);
            for group in 0..p.map.num_layouts() {
                for locked in [0, caps] {
                    let mut state = p.state([0, 0, locked, group], 0);
                    for value in b' '..=b'~' {
                        let key = symbol(u32::from(value));
                        let mods = [
                            state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                            state.serialize_mods(xkb::STATE_MODS_LATCHED),
                            state.serialize_mods(xkb::STATE_MODS_LOCKED),
                            state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
                        ];
                        let events = p.press(key, mods).unwrap_or_else(|error| {
                            panic!(
                                "{layout} group{group} locked{locked} char{}: {error:?}",
                                char::from(value)
                            )
                        });
                        for &(code, pressed) in &events {
                            assert!(pressed);
                            state.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Down);
                        }
                        let code = events.last().unwrap().0;
                        assert_eq!(
                            state.key_get_one_sym(xkb::Keycode::new(code + 8)).raw(),
                            u32::from(value)
                        );
                        for (code, pressed) in p.release(key).unwrap() {
                            assert!(!pressed);
                            state.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Up);
                        }
                        assert_eq!(state.serialize_mods(xkb::STATE_MODS_DEPRESSED), 0);
                        assert_eq!(state.serialize_mods(xkb::STATE_MODS_LOCKED), locked);
                        assert_eq!(state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE), group);
                    }
                }
            }
        }
    }
    #[test]
    fn current_group_and_caps_lock_are_used_without_toggling_locks() {
        let mut p = fixture("us,fr");
        assert_eq!(
            p.press(symbol(b'a' as u32), [0, 0, 0, 1]),
            Ok(vec![(16, true)])
        );
        p.release(symbol(b'a' as u32)).unwrap();
        let caps = 1 << p.map.mod_get_index(&xkb::MOD_NAME_CAPS);
        assert_eq!(
            p.press(symbol(b'a' as u32), [0, 0, caps, 1]),
            Ok(vec![(42, true), (16, true)])
        );
        assert_eq!(
            p.release(symbol(b'a' as u32)),
            Ok(vec![(16, false), (42, false)])
        );
    }
    #[test]
    fn missing_symbols_and_invalid_transitions_leave_existing_ownership_intact() {
        let mut p = fixture("us");
        p.press(Key::Code(42), [0; 4]).unwrap();
        for key in [Key::Code(0), Key::Code(0x300), symbol(0), symbol(u32::MAX)] {
            assert_eq!(p.press(key, [0; 4]), Err(Error::Invalid));
        }
        assert_eq!(p.press(Key::Code(42), [0; 4]), Err(Error::Invalid));
        assert_eq!(p.release(symbol(b'a' as u32)), Err(Error::Invalid));
        assert_eq!(
            p.press(symbol(0x0101_f600), [0; 4]),
            Err(Error::Unavailable)
        );
        assert_eq!(p.release(Key::Code(42)), Ok(vec![(42, false)]));
        assert!(p.held.is_empty() && p.references.is_empty());
    }
    #[test]
    fn logical_and_physical_keys_are_bounded() {
        let mut p = fixture("us");
        for code in 1..=64 {
            p.press(Key::Code(code), [0; 4]).unwrap();
        }
        assert_eq!(p.press(Key::Code(65), [0; 4]), Err(Error::Limit));
        for code in 1..=64 {
            assert_eq!(p.release(Key::Code(code)), Ok(vec![(code, false)]));
        }
    }
}
