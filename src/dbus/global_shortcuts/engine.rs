//! Approved logical definitions and live compositor authority have different
//! lifetimes. Keymap/reservation replacement retires the old pressed recipients
//! before atomically publishing the newly admitted sets on the compositor thread.
use std::collections::{BTreeMap, BTreeSet};

use super::presses::{Edge, Error as PressError, Update};
use super::registry::{
    Conflict, Error as RegistryError, Slot, MAX_BINDINGS, MAX_KEYCODES_PER_LEASE,
};
use super::resolver::{Error as ResolveError, Snapshot};
use super::state::{Changed, State};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub id: u32,
    pub trigger: String,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    StaleGeneration,
    InvalidDefinitions,
    Resolution(Vec<(u32, ResolveError)>),
    Registration(RegistryError),
}
#[derive(Debug, PartialEq, Eq)]
pub struct Description {
    pub generation: u64,
    pub bindings: Vec<Definition>,
}
pub struct Revalidated {
    pub lease: u64,
    pub description: Description,
    pub deactivated: Vec<Edge>,
    pub failed: Vec<u32>,
}
#[derive(Default)]
struct Lease {
    approved: Vec<Definition>,
    current: Vec<Definition>,
    generation: u64,
}
#[derive(Default)]
pub struct Engine {
    state: State,
    map: Option<Snapshot>,
    leases: BTreeMap<u64, Lease>,
}
impl Engine {
    pub fn open(&mut self) -> Result<u64, RegistryError> {
        let id = self.state.open()?;
        self.leases.insert(id, Lease::default());
        Ok(id)
    }
    pub fn describe(&self, id: u64) -> Option<Description> {
        self.leases.get(&id).map(|lease| Description {
            generation: lease.generation,
            bindings: lease.current.clone(),
        })
    }
    pub fn close(&mut self, id: u64, time_us: u64) -> Vec<Edge> {
        self.leases.remove(&id);
        self.state.close(id, time_us)
    }
    pub fn replace(
        &mut self,
        id: u64,
        expected_generation: u64,
        proposed: Vec<Definition>,
        reserved: impl FnMut(Slot, u8) -> bool,
        time_us: u64,
    ) -> Result<Changed, Error> {
        let lease = self
            .leases
            .get(&id)
            .ok_or(Error::Registration(RegistryError::UnknownLease))?;
        if lease.generation != expected_generation {
            return Err(Error::StaleGeneration);
        }
        let mut ids = BTreeSet::new();
        if proposed.len() > MAX_BINDINGS || proposed.iter().any(|d| d.id == 0 || !ids.insert(d.id))
        {
            return Err(Error::InvalidDefinitions);
        }
        let map = self.map.as_ref().ok_or(Error::Unavailable)?;
        let mut bindings = Vec::new();
        let mut errors = Vec::new();
        for definition in &proposed {
            match map.resolve(definition.id, &definition.trigger) {
                Ok(binding) => bindings.push(binding),
                Err(error) => errors.push((definition.id, error)),
            }
        }
        if !errors.is_empty() {
            return Err(Error::Resolution(errors));
        }
        let committed = self
            .state
            .replace(id, bindings, reserved, time_us)
            .map_err(Error::Registration)?;
        let lease = self.leases.get_mut(&id).unwrap();
        lease.approved = proposed.clone();
        lease.current = proposed;
        lease.generation = committed.generation;
        Ok(committed)
    }
    /// Run without yielding to input dispatch. Clear every old live set first,
    /// then readmit approved definitions in lease creation order. A new keymap
    /// cannot make one lease's obsolete physical codes reserve another's trigger.
    /// Failed actions remain approved for a later compatible map/reservation set;
    /// they have no live authority until that later revalidation succeeds.
    pub fn revalidate(
        &mut self,
        map: Option<Snapshot>,
        mut reserved: impl FnMut(Slot, u8) -> bool,
        time_us: u64,
    ) -> Vec<Revalidated> {
        self.map = map;
        let ids: Vec<_> = self.leases.keys().copied().collect();
        let mut retired = BTreeMap::new();
        for &id in &ids {
            match self.state.replace(id, Vec::new(), |_, _| false, time_us) {
                Ok(change) => {
                    let lease = self.leases.get_mut(&id).unwrap();
                    lease.current.clear();
                    lease.generation = change.generation;
                    retired.insert(id, change.deactivated);
                }
                Err(_) => {
                    // Exhausted generations must never alias an older target.
                    // A missing description tells the transport to close this FD.
                    let edges = self.close(id, time_us);
                    retired.insert(id, edges);
                }
            }
        }
        let mut changed = Vec::new();
        for id in ids {
            let deactivated = retired.remove(&id).unwrap_or_default();
            let Some(lease) = self.leases.get(&id) else {
                changed.push(Revalidated {
                    lease: id,
                    description: Description {
                        generation: 0,
                        bindings: Vec::new(),
                    },
                    deactivated,
                    failed: Vec::new(),
                });
                continue;
            };
            let approved = lease.approved.clone();
            let mut failed = BTreeSet::new();
            let mut resolved = Vec::new();
            let mut slots = 0;
            for definition in &approved {
                match self
                    .map
                    .as_ref()
                    .map(|map| map.resolve(definition.id, &definition.trigger))
                {
                    Some(Ok(binding)) if slots + binding.keys.len() <= MAX_KEYCODES_PER_LEASE => {
                        slots += binding.keys.len();
                        resolved.push(binding);
                    }
                    _ => {
                        failed.insert(definition.id);
                    }
                }
            }
            // Conflict replies are bounded to one per action. Removing all
            // refused actions and retrying cannot retire a successful new set:
            // failed Registry::replace calls leave the registry untouched.
            loop {
                match self
                    .state
                    .replace(id, resolved.clone(), &mut reserved, time_us)
                {
                    Ok(change) => {
                        debug_assert!(change.deactivated.is_empty());
                        let lease = self.leases.get_mut(&id).unwrap();
                        lease.current = approved
                            .iter()
                            .filter(|d| !failed.contains(&d.id))
                            .cloned()
                            .collect();
                        lease.generation = change.generation;
                        break;
                    }
                    Err(RegistryError::Conflicts(conflicts)) => {
                        let before = resolved.len();
                        for conflict in conflicts {
                            let id = match conflict {
                                Conflict::Reserved { binding }
                                | Conflict::Occupied { binding, .. }
                                | Conflict::DuplicateTrigger { binding, .. } => binding,
                            };
                            failed.insert(id);
                        }
                        resolved.retain(|b| !failed.contains(&b.id));
                        if resolved.len() == before {
                            // A contradictory admission result is terminal;
                            // never spin in the compositor event loop.
                            self.close(id, time_us);
                            break;
                        }
                    }
                    Err(_) => {
                        self.close(id, time_us);
                        break;
                    }
                }
            }
            changed.push(Revalidated {
                lease: id,
                description: self.describe(id).unwrap_or(Description {
                    generation: 0,
                    bindings: Vec::new(),
                }),
                deactivated,
                failed: failed.into_iter().collect(),
            });
        }
        changed
    }
    pub fn raw_edge(&mut self, source: u64, code: u32, pressed: bool, time_us: u64) -> Update {
        self.state.raw_edge(source, code, pressed, time_us)
    }
    /// Call inside the actual combined-seat press filter, after ordinary/a11y,
    /// lock, inhibitor and compositor UI policy. Releases never resolve again.
    pub fn accepted_press(
        &mut self,
        source: u64,
        code: u32,
        layout: u32,
        effective_mods: u32,
        time_us: u64,
    ) -> Result<Option<Update>, PressError> {
        let Some(map) = self.map.as_ref() else {
            return Ok(None);
        };
        self.state.accepted_press(
            source,
            &map.candidates(code, layout),
            map.logical_modifiers(effective_mods),
            time_us,
        )
    }
    pub fn seat_key(&mut self, code: u32, down: bool, time_us: u64) -> Vec<Edge> {
        self.state.seat_key(code, down, time_us)
    }
    pub fn source_closed(&mut self, source: u64, time_us: u64) -> Vec<Edge> {
        self.state.source_closed(source, time_us)
    }
    pub fn reconcile(&mut self, down: &BTreeSet<u32>, time_us: u64) -> Vec<Edge> {
        self.state.reconcile(down, time_us)
    }
    /// Stop active actions on lock/inhibition/UI transitions, while retaining
    /// suppression of the held cycles and approved registrations.
    pub fn suspend(&mut self, time_us: u64) -> Vec<Edge> {
        self.state.suspend(time_us)
    }
}

#[cfg(test)]
mod tests {
    use super::super::xkb;
    use super::*;
    fn keymap(layout: &str) -> xkb::Keymap {
        xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            "evdev",
            "pc105",
            layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap()
    }
    fn snapshot(layout: &str) -> Snapshot {
        Snapshot::new(&keymap(layout)).unwrap()
    }
    fn definition(id: u32, trigger: &str) -> Definition {
        Definition {
            id,
            trigger: trigger.into(),
        }
    }
    fn ready() -> (Engine, u64) {
        let mut e = Engine::default();
        e.revalidate(Some(snapshot("us")), |_, _| false, 0);
        let id = e.open().unwrap();
        e.replace(
            id,
            e.describe(id).unwrap().generation,
            vec![definition(1, "a")],
            |_, _| false,
            1000,
        )
        .unwrap();
        (e, id)
    }
    #[test]
    fn invalid_definition_batch_preserves_existing_pressed_action() {
        let (mut e, id) = ready();
        let old = e.accepted_press(1, 38, 0, 0, 2000).unwrap().unwrap().edges[0];
        let previous = e.describe(id).unwrap();
        assert!(matches!(
            e.replace(
                id,
                e.describe(id).unwrap().generation,
                vec![definition(1, "q"), definition(1, "w")],
                |_, _| false,
                3000
            ),
            Err(Error::InvalidDefinitions)
        ));
        assert!(matches!(
            e.replace(
                id,
                e.describe(id).unwrap().generation,
                vec![definition(1, "CTRL+invalid_symbol")],
                |_, _| false,
                3000
            ),
            Err(Error::Resolution(_))
        ));
        assert_eq!(e.describe(id).unwrap(), previous);
        assert_eq!(e.raw_edge(1, 38, false, 4000).edges[0].target, old.target);
    }
    #[test]
    fn keymap_change_retires_original_recipient_and_matches_the_new_physical_key() {
        let (mut e, id) = ready();
        let old = e.accepted_press(1, 38, 0, 0, 2000).unwrap().unwrap().edges[0];
        let changes = e.revalidate(Some(snapshot("fr")), |_, _| false, 3000);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].lease, id);
        assert_eq!(changes[0].deactivated[0].target, old.target);
        assert!(!changes[0].deactivated[0].activated);
        assert_eq!(changes[0].description.bindings, vec![definition(1, "a")]);
        assert!(changes[0].description.generation > old.target.generation);
        assert!(e.raw_edge(1, 38, false, 4000).edges.is_empty());
        e.seat_key(38, false, 4000);
        assert!(e.accepted_press(1, 38, 0, 0, 5000).unwrap().is_none());
        assert_eq!(
            e.accepted_press(1, 24, 0, 0, 6000).unwrap().unwrap().edges[0]
                .target
                .binding,
            1
        );
    }
    #[test]
    fn new_reservation_removes_only_conflicting_actions_and_later_restores_approved_choices() {
        let (mut e, id) = ready();
        e.replace(
            id,
            e.describe(id).unwrap().generation,
            vec![definition(1, "a"), definition(2, "q")],
            |_, _| false,
            2000,
        )
        .unwrap();
        e.accepted_press(1, 38, 0, 0, 3000).unwrap();
        let changes = e.revalidate(Some(snapshot("us")), |slot, _| slot.code == 38, 4000);
        assert_eq!(changes[0].failed, vec![1]);
        assert_eq!(changes[0].deactivated.len(), 1);
        assert_eq!(e.describe(id).unwrap().bindings, vec![definition(2, "q")]);
        let changes = e.revalidate(Some(snapshot("us")), |_, _| false, 5000);
        assert!(changes[0].failed.is_empty());
        assert_eq!(e.describe(id).unwrap().bindings.len(), 2);
        assert!(e.raw_edge(2, 38, true, 6000).captured);
        assert!(e
            .accepted_press(2, 38, 0, 0, 6000)
            .unwrap()
            .unwrap()
            .edges
            .is_empty());
        e.raw_edge(1, 38, false, 7000);
        e.raw_edge(2, 38, false, 8000);
        e.seat_key(38, false, 8000);
        assert!(e.accepted_press(2, 38, 0, 0, 9000).unwrap().unwrap().edges[0].activated);
    }
    #[test]
    fn unavailable_keymap_clears_authority_but_does_not_reuse_or_replace_the_lease() {
        let (mut e, id) = ready();
        e.accepted_press(1, 38, 0, 0, 2000).unwrap();
        let changes = e.revalidate(None, |_, _| false, 3000);
        assert_eq!(changes[0].deactivated.len(), 1);
        assert_eq!(changes[0].failed, vec![1]);
        assert!(e.describe(id).unwrap().bindings.is_empty());
        assert!(e.accepted_press(1, 24, 0, 0, 4000).unwrap().is_none());
        assert_eq!(
            e.replace(
                id,
                e.describe(id).unwrap().generation,
                vec![],
                |_, _| false,
                4000
            )
            .err(),
            Some(Error::Unavailable)
        );
        let other = e.open().unwrap();
        assert!(other > id);
        e.revalidate(Some(snapshot("fr")), |_, _| false, 5000);
        assert_eq!(e.describe(id).unwrap().bindings, vec![definition(1, "a")]);
        assert!(e.describe(other).unwrap().bindings.is_empty());
        e.close(id, 6000);
        assert!(e.describe(id).is_none());
        assert!(e.open().unwrap() > other);
    }
    #[test]
    fn source_loss_ends_only_the_last_captured_holder_with_source_time() {
        let (mut e, _) = ready();
        e.accepted_press(11, 38, 0, 0, 2000).unwrap();
        assert!(e.raw_edge(12, 38, true, 3000).captured);
        assert!(e.source_closed(11, 4000).is_empty());
        assert!(e.seat_key(38, true, 4000).is_empty());
        let edges = e.source_closed(12, 5_000_000_123_000);
        assert_eq!(edges.len(), 1);
        assert!(!edges[0].activated);
        assert_eq!(edges[0].timestamp_ms, 5_000_000_123);
        assert!(e.seat_key(38, false, 5_000_000_123_000).is_empty());
    }
    #[test]
    fn suspension_deactivates_once_without_allowing_mid_hold_recapture() {
        let (mut e, _) = ready();
        e.accepted_press(1, 38, 0, 0, 2000).unwrap();
        assert_eq!(e.suspend(3000).len(), 1);
        assert!(e.suspend(4000).is_empty());
        assert!(e.raw_edge(1, 38, true, 5000).captured);
        assert!(e
            .accepted_press(1, 38, 0, 0, 5000)
            .unwrap()
            .unwrap()
            .edges
            .is_empty());
        assert!(e.raw_edge(1, 38, false, 6000).edges.is_empty());
        e.seat_key(38, false, 6000);
        assert!(e.accepted_press(1, 38, 0, 0, 7000).unwrap().unwrap().edges[0].activated);
    }
    #[test]
    fn stale_configuration_cannot_replace_a_newly_revalidated_live_action() {
        let (mut e, id) = ready();
        let observed = e.describe(id).unwrap().generation;
        e.revalidate(Some(snapshot("fr")), |_, _| false, 2000);
        let current = e.describe(id).unwrap();
        let press = e.accepted_press(1, 24, 0, 0, 3000).unwrap().unwrap().edges[0];
        assert_eq!(
            e.replace(id, observed, vec![definition(1, "q")], |_, _| false, 4000)
                .err(),
            Some(Error::StaleGeneration)
        );
        assert_eq!(e.describe(id).unwrap(), current);
        assert_eq!(e.raw_edge(1, 24, false, 5000).edges[0].target, press.target);
    }
}
