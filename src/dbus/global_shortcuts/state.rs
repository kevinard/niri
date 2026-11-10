//! One ownership boundary for registration changes and captured press retirement.
//! The compositor adapter supplies resolved bindings, current reservations and
//! actual seat/source edges; transport and application consent remain outside.
use super::presses::{Edge, Error as PressError, Presses, Update};
use super::registry::{Binding, Error as RegistryError, Registry, Slot};

#[derive(Default)]
pub struct State {
    registry: Registry,
    presses: Presses,
}
pub struct Changed {
    pub generation: u64,
    pub deactivated: Vec<Edge>,
}
impl State {
    pub fn open(&mut self) -> Result<u64, RegistryError> {
        self.registry.open()
    }
    pub fn replace(
        &mut self,
        lease: u64,
        bindings: Vec<Binding>,
        reserved: impl FnMut(Slot, u8) -> bool,
        time_us: u64,
    ) -> Result<Changed, RegistryError> {
        let committed = self.registry.replace(lease, bindings, reserved)?;
        let deactivated = self.presses.retire(committed.retired, time_us);
        Ok(Changed {
            generation: committed.generation,
            deactivated,
        })
    }
    pub fn close(&mut self, lease: u64, time_us: u64) -> Vec<Edge> {
        self.registry
            .close(lease)
            .map(|targets| self.presses.retire(targets, time_us))
            .unwrap_or_default()
    }
    pub fn source_closed(&mut self, source: u64, time_us: u64) -> Vec<Edge> {
        self.presses.remove_source(source, time_us)
    }
    pub fn suspend(&mut self, time_us: u64) -> Vec<Edge> {
        self.presses.retire(self.registry.all_targets(), time_us)
    }
    pub fn reconcile(&mut self, down: &std::collections::BTreeSet<u32>, time_us: u64) -> Vec<Edge> {
        self.presses
            .keys()
            .into_iter()
            .flat_map(|key| self.presses.finish_key(key, down.contains(&key), time_us))
            .collect()
    }
    pub fn raw_edge(&mut self, source: u64, key: u32, pressed: bool, time_us: u64) -> Update {
        self.presses.continue_cycle(source, key, pressed, time_us)
    }
    /// Only call for an actual new combined-seat press after ordinary compositor
    /// input policy has allowed portal matching. A raw duplicate joins via raw_edge.
    pub fn accepted_press(
        &mut self,
        source: u64,
        keys: &[Slot],
        modifiers: u8,
        time_us: u64,
    ) -> Result<Option<Update>, PressError> {
        let Some(first) = keys.first() else {
            return Ok(None);
        };
        if keys.iter().any(|slot| slot.code != first.code) {
            return Err(PressError::Invalid);
        }
        keys.iter()
            .find_map(|&slot| self.registry.lookup(slot, modifiers))
            .map(|target| self.presses.begin(source, first.code, target, time_us))
            .transpose()
    }
    pub fn seat_key(&mut self, key: u32, down: bool, time_us: u64) -> Vec<Edge> {
        self.presses.finish_key(key, down, time_us)
    }
}
#[cfg(test)]
mod tests {
    use super::super::registry::{ALT, CTRL};
    use super::*;
    fn binding(key: u32, modifiers: u8) -> Binding {
        Binding {
            id: 7,
            keys: vec![Slot::new(key, 0)],
            modifiers,
        }
    }
    #[test]
    fn refused_reconfiguration_preserves_a_live_press_and_its_real_release() {
        let mut s = State::default();
        let lease = s.open().unwrap();
        s.replace(lease, vec![binding(38, CTRL)], |_, _| false, 0)
            .unwrap();
        let press = s
            .accepted_press(1, &[Slot::new(38, 0)], CTRL, 1000)
            .unwrap()
            .unwrap()
            .edges[0];
        assert!(press.activated);
        assert!(s
            .replace(lease, vec![binding(40, ALT)], |_, _| true, 2000)
            .is_err());
        let released = s.raw_edge(1, 38, false, 3000);
        assert_eq!(released.edges.len(), 1);
        assert_eq!(released.edges[0].target, press.target);
        assert!(!released.edges[0].activated);
        assert!(s
            .accepted_press(2, &[Slot::new(40, 0)], ALT, 4000)
            .unwrap()
            .is_none());
    }
    #[test]
    fn committed_reconfiguration_retires_old_generation_before_a_fresh_press() {
        let mut s = State::default();
        let lease = s.open().unwrap();
        s.replace(lease, vec![binding(38, CTRL)], |_, _| false, 0)
            .unwrap();
        let press = s
            .accepted_press(1, &[Slot::new(38, 0)], CTRL, 1000)
            .unwrap()
            .unwrap()
            .edges[0];
        let change = s
            .replace(lease, vec![binding(38, ALT)], |_, _| false, 2000)
            .unwrap();
        assert_eq!(change.deactivated.len(), 1);
        assert_eq!(change.deactivated[0].target, press.target);
        assert!(s.raw_edge(1, 38, false, 3000).edges.is_empty());
        s.seat_key(38, false, 3000);
        assert!(s
            .accepted_press(1, &[Slot::new(38, 0)], CTRL, 4000)
            .unwrap()
            .is_none());
        let new = s
            .accepted_press(1, &[Slot::new(38, 0)], ALT, 5000)
            .unwrap()
            .unwrap()
            .edges[0];
        assert_eq!(new.target.generation, change.generation);
        assert_ne!(new.target, press.target);
    }
    #[test]
    fn closed_lease_cannot_revive_and_keeps_a_held_tombstone_until_seat_up() {
        let mut s = State::default();
        let lease = s.open().unwrap();
        s.replace(lease, vec![binding(38, CTRL)], |_, _| false, 0)
            .unwrap();
        s.accepted_press(1, &[Slot::new(38, 0)], CTRL, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(s.close(lease, 2000).len(), 1);
        assert!(s.close(lease, 2500).is_empty());
        let next = s.open().unwrap();
        s.replace(next, vec![binding(38, CTRL)], |_, _| false, 3000)
            .unwrap();
        assert!(s.raw_edge(2, 38, true, 4000).captured);
        assert!(s
            .accepted_press(2, &[Slot::new(38, 0)], CTRL, 4000)
            .unwrap()
            .unwrap()
            .edges
            .is_empty());
        assert!(s.raw_edge(1, 38, false, 5000).edges.is_empty());
        assert!(s.raw_edge(2, 38, false, 6000).edges.is_empty());
        s.seat_key(38, false, 6000);
        let new = s
            .accepted_press(2, &[Slot::new(38, 0)], CTRL, 7000)
            .unwrap()
            .unwrap()
            .edges[0];
        assert_eq!(new.target.lease, next);
    }
    #[test]
    fn a_layout_switch_during_a_hold_cannot_redirect_its_release_to_another_action() {
        let mut state = State::default();
        let lease = state.open().unwrap();
        let a = Binding {
            id: 7,
            keys: vec![Slot::new(38, 0)],
            modifiers: CTRL,
        };
        let b = Binding {
            id: 8,
            keys: vec![Slot::new(38, 1)],
            modifiers: CTRL,
        };
        state.replace(lease, vec![a, b], |_, _| false, 0).unwrap();
        let old = state
            .accepted_press(1, &[Slot::new(38, 0)], CTRL, 1000)
            .unwrap()
            .unwrap()
            .edges[0];
        assert_eq!(old.target.binding, 7);
        let release = state.raw_edge(1, 38, false, 2000).edges;
        assert_eq!(release.len(), 1);
        assert_eq!(release[0].target, old.target);
        state.seat_key(38, false, 2000);
        let new = state
            .accepted_press(1, &[Slot::new(38, 1)], CTRL, 3000)
            .unwrap()
            .unwrap()
            .edges[0];
        assert_eq!(new.target.binding, 8);
    }
}
