//! Explicit virtual-keyboard modifier ownership across capture boundaries.
use std::collections::BTreeMap;

pub type Modifiers = (u32, u32, u32, u32);
const MAX_SOURCES: usize = 256;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Exhausted,
    Phase,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub ordinary: Option<Modifiers>,
    pub captured: Option<Modifiers>,
}
#[derive(Default)]
struct Source {
    local: [u32; 3],
    captured: u32,
    suppressed: u32,
    pending: bool,
    participated: bool,
}
#[derive(Default)]
pub struct Masks {
    sources: BTreeMap<u64, Source>,
    active: bool,
    exhausted: bool,
}
impl Masks {
    pub fn activate(&mut self) -> Result<(), Refusal> {
        if self.exhausted {
            return Err(Refusal::Exhausted);
        }
        if self.active {
            return Err(Refusal::Phase);
        }
        self.active = true;
        Ok(())
    }
    pub fn retire(&mut self, raw_tail: impl Fn(u64) -> bool) {
        self.active = false;
        for (&id, source) in &mut self.sources {
            let held = source.captured != 0 || raw_tail(id);
            source.suppressed |= source.captured;
            source.captured = 0;
            source.pending |= held;
            source.participated = false;
        }
    }
    fn source(&mut self, id: u64) -> Result<&mut Source, Refusal> {
        if !self.sources.contains_key(&id) && self.sources.len() >= MAX_SOURCES {
            self.exhausted = true;
            return Err(Refusal::Exhausted);
        }
        Ok(self.sources.entry(id).or_default())
    }
    pub fn record_capture(&mut self, id: u64, raw_tail: bool) -> Result<(), Refusal> {
        if !self.active {
            return Err(Refusal::Phase);
        }
        let source = self.source(id)?;
        source.participated = true;
        if !raw_tail {
            source.pending = false;
        }
        Ok(())
    }
    pub fn local_input(&mut self, id: u64) {
        if let Some(source) = self.sources.get_mut(&id) {
            source.pending = false;
        }
    }
    pub fn participates(&self, id: u64) -> bool {
        self.active
            && self
                .sources
                .get(&id)
                .is_some_and(|source| source.participated)
    }
    pub fn remove_source(&mut self, id: u64) {
        self.sources.remove(&id);
    }
    pub fn local_depressed(&self) -> u32 {
        self.sources
            .values()
            .fold(0, |mask, source| mask | source.local[0])
    }
    pub fn update(
        &mut self,
        id: u64,
        input: Modifiers,
        main: Modifiers,
        capture: Option<Modifiers>,
        raw_tail: bool,
    ) -> Result<Output, Refusal> {
        if self.active != capture.is_some() {
            return Err(Refusal::Phase);
        }
        let source = self.source(id)?;
        let old_local = source.local;
        let old_suppressed = source.suppressed;
        let quarantine = source.pending || raw_tail;
        let seen = source.local[0] | source.captured | source.suppressed;
        source.local[0] &= input.0;
        source.captured &= input.0;
        source.suppressed &= input.0;
        let new = input.0 & !seen;
        if quarantine {
            source.suppressed |= new;
        } else if capture.is_some() {
            source.captured |= new;
        } else {
            source.local[0] |= new;
        }
        if let Some(current) = capture {
            source.participated = true;
            source.local[1] &= input.1;
            source.local[2] &= input.2;
            let ordinary = (old_local != [0; 3] || source.local != [0; 3]).then_some((
                (main.0 & !old_local[0]) | source.local[0],
                (main.1 & !old_local[1]) | source.local[1],
                (main.2 & !old_local[2]) | source.local[2],
                main.3,
            ));
            let depressed = if quarantine {
                (current.0 & !old_local[0]) | source.local[0] | source.captured
            } else {
                source.local[0] | source.captured
            };
            if !raw_tail && input.0 == 0 {
                source.pending = false;
            }
            Ok(Output {
                ordinary,
                captured: Some((depressed, input.1, input.2, input.3)),
            })
        } else {
            source.local[1] = input.1;
            source.local[2] = input.2;
            let depressed = if quarantine || old_suppressed != 0 || source.suppressed != 0 {
                (main.0 & !old_local[0]) | source.local[0]
            } else {
                input.0
            };
            if !raw_tail && input.0 == 0 {
                source.pending = false;
            }
            Ok(Output {
                ordinary: Some((depressed, input.1, input.2, input.3)),
                captured: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ZERO: Modifiers = (0, 0, 0, 0);
    #[test]
    fn old_local_control_can_clear_without_importing_new_captured_shift() {
        let mut masks = Masks::default();
        assert_eq!(
            masks
                .update(1, (4, 0, 0, 0), ZERO, None, false)
                .unwrap()
                .ordinary,
            Some((4, 0, 0, 0))
        );
        assert_eq!(masks.local_depressed(), 4);
        masks.activate().unwrap();
        let result = masks
            .update(1, (5, 0, 0, 0), (4, 0, 0, 0), Some((5, 0, 0, 0)), false)
            .unwrap();
        assert_eq!(result.ordinary, Some((4, 0, 0, 0)));
        assert_eq!(result.captured, Some((5, 0, 0, 0)));
        let result = masks
            .update(1, (1, 0, 0, 0), (4, 0, 0, 0), Some((5, 0, 0, 0)), false)
            .unwrap();
        assert_eq!(result.ordinary, Some(ZERO));
        assert_eq!(result.captured, Some((1, 0, 0, 0)));
    }
    #[test]
    fn captured_modifier_only_updates_stay_suppressed_through_stop_and_a_new_activation() {
        let mut masks = Masks::default();
        masks.activate().unwrap();
        assert_eq!(
            masks
                .update(1, (12, 0, 0, 0), ZERO, Some(ZERO), false)
                .unwrap()
                .ordinary,
            None
        );
        masks.retire(|_| false);
        assert_eq!(
            masks
                .update(1, (4, 0, 0, 0), ZERO, None, false)
                .unwrap()
                .ordinary,
            Some(ZERO)
        );
        masks.activate().unwrap();
        let result = masks
            .update(1, (4, 0, 0, 0), ZERO, Some(ZERO), false)
            .unwrap();
        assert_eq!(result.captured, Some(ZERO));
        assert_eq!(result.ordinary, None);
        masks.update(1, ZERO, ZERO, Some(ZERO), false).unwrap();
        let result = masks
            .update(1, (1, 0, 0, 0), ZERO, Some(ZERO), false)
            .unwrap();
        assert_eq!(result.captured, Some((1, 0, 0, 0)));
    }
    #[test]
    fn first_late_modifier_packet_after_a_raw_captured_key_does_not_create_a_local_press() {
        let mut masks = Masks::default();
        masks.activate().unwrap();
        masks.record_capture(1, false).unwrap();
        masks.retire(|id| id == 1);
        assert_eq!(
            masks
                .update(1, (4, 0, 0, 0), ZERO, None, true)
                .unwrap()
                .ordinary,
            Some(ZERO)
        );
        // Clearing the old modifier must preserve an unrelated local Shift.
        assert_eq!(
            masks
                .update(1, ZERO, (1, 0, 0, 0), None, false)
                .unwrap()
                .ordinary,
            Some((1, 0, 0, 0))
        );
        masks.local_input(1);
        assert_eq!(
            masks
                .update(1, (8, 0, 0, 0), ZERO, None, false)
                .unwrap()
                .ordinary,
            Some((8, 0, 0, 0))
        );
    }
    #[test]
    fn a_fresh_raw_key_allows_new_modifiers_without_adopting_old_held_bits() {
        let mut masks = Masks::default();
        masks.activate().unwrap();
        masks
            .update(1, (4, 0, 0, 0), ZERO, Some(ZERO), false)
            .unwrap();
        masks.retire(|_| false);
        masks.activate().unwrap();
        masks.record_capture(1, false).unwrap();
        let result = masks
            .update(1, (5, 0, 0, 0), ZERO, Some((1, 0, 0, 0)), false)
            .unwrap();
        assert_eq!(result.captured, Some((1, 0, 0, 0)));
        assert_eq!(result.ordinary, None);
    }
    #[test]
    fn source_removal_and_bounded_admission_do_not_clear_other_sources() {
        let mut masks = Masks::default();
        for id in 1..=MAX_SOURCES as u64 {
            masks.update(id, (4, 0, 0, 0), ZERO, None, false).unwrap();
        }
        assert_eq!(
            masks.update(MAX_SOURCES as u64 + 1, ZERO, ZERO, None, false),
            Err(Refusal::Exhausted)
        );
        assert_eq!(masks.activate(), Err(Refusal::Exhausted));
        masks.remove_source(1);
        assert_eq!(masks.local_depressed(), 4);
        for id in 2..=MAX_SOURCES as u64 {
            masks.remove_source(id);
        }
        assert_eq!(masks.local_depressed(), 0);
    }
    #[test]
    fn normal_absolute_virtual_mask_and_group_semantics_are_unchanged_outside_capture() {
        let mut masks = Masks::default();
        assert_eq!(
            masks
                .update(1, (0, 1, 2, 3), (7, 8, 9, 0), None, false)
                .unwrap()
                .ordinary,
            Some((0, 1, 2, 3))
        );
        assert!(!masks.participates(1));
        masks.activate().unwrap();
        assert_eq!(masks.activate(), Err(Refusal::Phase));
        assert_eq!(
            masks.update(1, ZERO, ZERO, None, false),
            Err(Refusal::Phase)
        );
        masks.record_capture(1, false).unwrap();
        assert!(masks.participates(1));
        masks.retire(|_| false);
        assert!(!masks.participates(1));
    }
    #[test]
    fn a_completed_key_does_not_quarantine_a_fresh_modifier_only_press_after_stop() {
        let mut masks = Masks::default();
        masks.activate().unwrap();
        masks.record_capture(1, false).unwrap();
        // The raw key finished before capture stopped, with no held mask.
        masks.retire(|_| false);
        assert_eq!(
            masks
                .update(1, (4, 0, 0, 0), ZERO, None, false)
                .unwrap()
                .ordinary,
            Some((4, 0, 0, 0))
        );
    }
}
