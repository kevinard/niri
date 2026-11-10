//! Compositor-owned touch contact identities, independent of transport and focus.
//!
//! A lease owns a Source, never a seat slot supplied by a client. Physical input
//! takes priority: apply returned retirements (up + frame and lease closure)
//! BEFORE forwarding its new down. Motion/up must consult the physical guards
//! so cancelled physical tails cannot release a newly allocated remote slot.
use std::collections::BTreeMap;

pub const MAX_SOURCES: usize = 16;
pub const MAX_CONTACTS_PER_SOURCE: usize = 32;
pub const SEAT_SLOTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Source(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot(u32);
impl Slot {
    pub fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    SourceLimit,
    IdentityExhausted,
    UnknownSource,
    UnknownContact,
    AlreadyDown,
    ContactLimit,
    SlotLimit,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Retired {
    pub source: Source,
    pub slots: Vec<Slot>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Physical {
    count: u32,
    overflow: bool,
}
impl Physical {
    fn active(self) -> bool {
        self.count != 0 || self.overflow
    }
}

pub struct Registry {
    next: u64,
    sources: BTreeMap<Source, BTreeMap<u32, Slot>>,
    // Only these slots can overlap our finite remote pool. Other physical IDs
    // pass through unchanged and need no new allocation or tracking here.
    physical: [Physical; SEAT_SLOTS],
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            next: 0,
            sources: BTreeMap::new(),
            physical: [Physical::default(); SEAT_SLOTS],
        }
    }
}

impl Registry {
    pub fn open(&mut self) -> Result<Source, Error> {
        if self.sources.len() >= MAX_SOURCES {
            return Err(Error::SourceLimit);
        }
        let next = self.next.checked_add(1).ok_or(Error::IdentityExhausted)?;
        let source = Source(next);
        self.sources.insert(source, BTreeMap::new());
        self.next = next;
        Ok(source)
    }

    pub fn down(&mut self, source: Source, contact: u32) -> Result<Slot, Error> {
        let contacts = self.sources.get(&source).ok_or(Error::UnknownSource)?;
        if contacts.contains_key(&contact) {
            return Err(Error::AlreadyDown);
        }
        if contacts.len() >= MAX_CONTACTS_PER_SOURCE {
            return Err(Error::ContactLimit);
        }
        let slot = (0..SEAT_SLOTS)
            .find(|slot| {
                !self.physical[*slot].active()
                    && !self
                        .sources
                        .values()
                        .flat_map(|contacts| contacts.values())
                        .any(|owned| owned.0 == *slot as u32)
            })
            .map(|slot| Slot(slot as u32))
            .ok_or(Error::SlotLimit)?;
        self.sources.get_mut(&source).unwrap().insert(contact, slot);
        Ok(slot)
    }

    pub fn motion(&self, source: Source, contact: u32) -> Result<Slot, Error> {
        self.sources
            .get(&source)
            .ok_or(Error::UnknownSource)?
            .get(&contact)
            .copied()
            .ok_or(Error::UnknownContact)
    }

    pub fn up(&mut self, source: Source, contact: u32) -> Result<Slot, Error> {
        self.sources
            .get_mut(&source)
            .ok_or(Error::UnknownSource)?
            .remove(&contact)
            .ok_or(Error::UnknownContact)
    }

    /// Idempotent source retirement. Release only these slots, never issue a
    /// seat-wide cancel to retire one remote owner.
    pub fn retire(&mut self, source: Source) -> Option<Retired> {
        let contacts = self.sources.remove(&source)?;
        let mut slots: Vec<_> = contacts.into_values().collect();
        slots.sort_unstable();
        Some(Retired { source, slots })
    }

    /// Lock/shutdown can withdraw remote leases without losing physical holds.
    pub fn retire_all(&mut self) -> Vec<Retired> {
        let sources: Vec<_> = self.sources.keys().copied().collect();
        sources
            .into_iter()
            .map(|source| self.retire(source).unwrap())
            .collect()
    }

    /// A colliding physical down withdraws the entire remote lease. Its old
    /// packets cannot be reinterpreted as a continuation of the physical touch.
    pub fn physical_down(&mut self, slot: i32) -> Option<Retired> {
        let index = physical_index(slot)?;
        let physical = &mut self.physical[index];
        if let Some(count) = physical.count.checked_add(1) {
            physical.count = count;
        } else {
            // Counter uncertainty must not turn an occupied slot into a free one.
            // A physical seat cancel is required to clear this reservation.
            physical.overflow = true;
        }
        let owner = self.sources.iter().find_map(|(source, contacts)| {
            contacts
                .values()
                .any(|owned| owned.0 == index as u32)
                .then_some(*source)
        });
        owner.and_then(|source| self.retire(source))
    }

    pub fn physical_motion_allowed(&self, slot: i32) -> bool {
        physical_index(slot).is_none_or(|index| self.physical[index].active())
    }

    /// False means a stale tail in our managed range: do not forward that up.
    pub fn physical_up(&mut self, slot: i32) -> bool {
        physical_index(slot).is_none_or(|index| {
            let physical = &mut self.physical[index];
            if physical.overflow {
                return true;
            }
            if physical.count == 0 {
                return false;
            }
            physical.count -= 1;
            true
        })
    }

    /// A physical seat cancel also invalidates all remote leases. Close them,
    /// then forward the cancel; it must not silently leave remote IDs live.
    pub fn physical_cancel(&mut self) -> Vec<Retired> {
        let retired = self.retire_all();
        self.physical.fill(Physical::default());
        retired
    }
}

fn physical_index(slot: i32) -> Option<usize> {
    usize::try_from(slot).ok().filter(|slot| *slot < SEAT_SLOTS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn snapshot(
        registry: &Registry,
    ) -> (
        u64,
        BTreeMap<Source, BTreeMap<u32, Slot>>,
        [Physical; SEAT_SLOTS],
    ) {
        (registry.next, registry.sources.clone(), registry.physical)
    }

    fn invariants(registry: &Registry) {
        assert!(registry.sources.len() <= MAX_SOURCES);
        let mut slots = BTreeSet::new();
        for contacts in registry.sources.values() {
            assert!(contacts.len() <= MAX_CONTACTS_PER_SOURCE);
            for slot in contacts.values() {
                assert!((slot.get() as usize) < SEAT_SLOTS);
                assert!(!registry.physical[slot.get() as usize].active());
                assert!(slots.insert(*slot), "Seat slot belongs to two contacts");
            }
        }
        assert!(slots.len() <= SEAT_SLOTS);
    }

    #[test]
    fn client_ids_are_namespaced_and_slots_reuse_only_after_up() {
        let mut registry = Registry::default();
        let a = registry.open().unwrap();
        let b = registry.open().unwrap();
        let first = registry.down(a, u32::MAX).unwrap();
        let second = registry.down(b, u32::MAX).unwrap();
        assert_ne!(first, second);
        assert_eq!(registry.motion(a, u32::MAX), Ok(first));
        assert_eq!(registry.up(a, u32::MAX), Ok(first));
        assert_eq!(registry.up(a, u32::MAX), Err(Error::UnknownContact));
        assert_eq!(registry.down(a, u32::MAX), Ok(first));
        assert_eq!(registry.motion(b, u32::MAX), Ok(second));
        invariants(&registry);
    }

    #[test]
    fn retirement_is_owner_only_and_late_packets_cannot_touch_reused_slots() {
        let mut registry = Registry::default();
        let a = registry.open().unwrap();
        let b = registry.open().unwrap();
        let a0 = registry.down(a, 9).unwrap();
        let a1 = registry.down(a, 1).unwrap();
        let b0 = registry.down(b, 9).unwrap();
        assert_eq!(
            registry.retire(a),
            Some(Retired {
                source: a,
                slots: vec![a0, a1]
            })
        );
        assert_eq!(registry.retire(a), None);
        let c = registry.open().unwrap();
        assert_ne!(a, c);
        assert_eq!(registry.down(c, 9), Ok(a0));
        assert_eq!(registry.up(a, 9), Err(Error::UnknownSource));
        assert_eq!(registry.motion(c, 9), Ok(a0));
        assert_eq!(registry.motion(b, 9), Ok(b0));
        invariants(&registry);
    }

    #[test]
    fn physical_holds_are_reserved_and_collision_withdraws_the_remote_owner() {
        let mut registry = Registry::default();
        assert_eq!(registry.physical_down(0), None);
        let a = registry.open().unwrap();
        let a0 = registry.down(a, 1).unwrap();
        assert_eq!(a0.get(), 1);
        assert!(registry.physical_up(0));
        let a1 = registry.down(a, 2).unwrap();
        assert_eq!(a1.get(), 0);
        let b = registry.open().unwrap();
        let b0 = registry.down(b, 1).unwrap();
        let retired = registry.physical_down(1).unwrap();
        assert_eq!(retired.source, a);
        assert_eq!(retired.slots, vec![a1, a0]);
        assert_eq!(registry.up(a, 1), Err(Error::UnknownSource));
        assert_eq!(registry.motion(b, 1), Ok(b0));
        assert!(registry.physical_motion_allowed(1));
        assert_eq!(registry.physical_down(1), None);
        let c = registry.open().unwrap();
        assert_eq!(registry.down(c, 1).unwrap().get(), 0);
        invariants(&registry);
    }

    #[test]
    fn seat_cancel_retires_all_sources_and_suppresses_physical_tails() {
        let mut registry = Registry::default();
        registry.physical_down(1);
        let a = registry.open().unwrap();
        let b = registry.open().unwrap();
        registry.down(a, 7).unwrap();
        registry.down(b, 7).unwrap();
        let retired = registry.physical_cancel();
        assert_eq!(retired.len(), 2);
        assert_eq!(retired[0].source, a);
        assert_eq!(retired[1].source, b);
        assert!(!registry.physical_motion_allowed(1));
        assert!(!registry.physical_up(1));
        let c = registry.open().unwrap();
        let slot = registry.down(c, 7).unwrap();
        assert!(!registry.physical_up(slot.get() as i32));
        assert!(!registry.physical_motion_allowed(slot.get() as i32));
        assert_eq!(registry.motion(c, 7), Ok(slot));
        assert_eq!(registry.up(a, 7), Err(Error::UnknownSource));
        assert_eq!(registry.up(b, 7), Err(Error::UnknownSource));
        invariants(&registry);
    }

    #[test]
    fn remote_shutdown_keeps_physical_reservations() {
        let mut registry = Registry::default();
        registry.physical_down(0);
        let a = registry.open().unwrap();
        let b = registry.open().unwrap();
        registry.down(a, 0).unwrap();
        registry.down(b, 0).unwrap();
        assert_eq!(registry.retire_all().len(), 2);
        assert!(registry.physical_motion_allowed(0));
        let c = registry.open().unwrap();
        assert_eq!(registry.down(c, 0).unwrap().get(), 1);
        invariants(&registry);
    }

    #[test]
    fn invalid_transitions_change_nothing() {
        let mut registry = Registry::default();
        let old = registry.open().unwrap();
        registry.retire(old);
        let source = registry.open().unwrap();
        registry.down(source, 5).unwrap();
        let before = snapshot(&registry);
        assert_eq!(registry.down(source, 5), Err(Error::AlreadyDown));
        assert_eq!(registry.down(old, 5), Err(Error::UnknownSource));
        assert_eq!(registry.motion(source, 6), Err(Error::UnknownContact));
        assert_eq!(registry.up(source, 6), Err(Error::UnknownContact));
        assert_eq!(registry.up(old, 5), Err(Error::UnknownSource));
        assert_eq!(snapshot(&registry), before);
    }

    #[test]
    fn source_contact_and_total_slot_limits_reclaim_after_retirement() {
        let mut registry = Registry::default();
        let sources: Vec<_> = (0..MAX_SOURCES).map(|_| registry.open().unwrap()).collect();
        let before = snapshot(&registry);
        assert_eq!(registry.open(), Err(Error::SourceLimit));
        assert_eq!(snapshot(&registry), before);
        for source in sources.iter().take(2) {
            for contact in 0..MAX_CONTACTS_PER_SOURCE as u32 {
                registry.down(*source, contact).unwrap();
            }
        }
        let before = snapshot(&registry);
        assert_eq!(registry.down(sources[0], 99), Err(Error::ContactLimit));
        assert_eq!(registry.down(sources[2], 99), Err(Error::SlotLimit));
        assert_eq!(snapshot(&registry), before);
        assert_eq!(
            registry.retire(sources[0]).unwrap().slots.len(),
            MAX_CONTACTS_PER_SOURCE
        );
        let new = registry.open().unwrap();
        assert!(new > *sources.last().unwrap());
        registry.down(new, 99).unwrap();
        invariants(&registry);
    }

    #[test]
    fn physical_contacts_can_exhaust_the_pool_without_remote_state_growth() {
        let mut registry = Registry::default();
        for slot in 0..SEAT_SLOTS {
            registry.physical_down(slot as i32);
        }
        let source = registry.open().unwrap();
        let before = snapshot(&registry);
        assert_eq!(registry.down(source, 0), Err(Error::SlotLimit));
        assert_eq!(snapshot(&registry), before);
        assert!(registry.physical_up(7));
        assert_eq!(registry.down(source, 0).unwrap().get(), 7);
        assert_eq!(registry.physical_down(7).unwrap().source, source);
        assert!(registry.physical_motion_allowed(7));
        invariants(&registry);
    }

    #[test]
    fn other_physical_ids_need_no_tracking_or_allocations() {
        let mut registry = Registry::default();
        let before = snapshot(&registry);
        for slot in [-1, SEAT_SLOTS as i32, i32::MIN, i32::MAX] {
            assert_eq!(registry.physical_down(slot), None);
            assert!(registry.physical_motion_allowed(slot));
            assert!(registry.physical_up(slot));
        }
        assert_eq!(snapshot(&registry), before);
    }

    #[test]
    fn overlapping_physical_devices_reserve_the_slot_until_the_last_up() {
        let mut registry = Registry::default();
        registry.physical_down(0);
        registry.physical_down(0);
        assert!(registry.physical_up(0));
        assert!(registry.physical_motion_allowed(0));
        let source = registry.open().unwrap();
        assert_eq!(registry.down(source, 0).unwrap().get(), 1);
        assert!(registry.physical_up(0));
        assert!(!registry.physical_motion_allowed(0));
        assert_eq!(registry.down(source, 1).unwrap().get(), 0);
    }

    #[test]
    fn physical_counter_uncertainty_cannot_free_a_slot_before_cancel() {
        let mut registry = Registry::default();
        registry.physical[0].count = u32::MAX;
        registry.physical_down(0);
        for _ in 0..3 {
            assert!(registry.physical_up(0));
            assert!(registry.physical_motion_allowed(0));
        }
        let source = registry.open().unwrap();
        assert_eq!(registry.down(source, 0).unwrap().get(), 1);
        registry.physical_cancel();
        assert!(!registry.physical_motion_allowed(0));
        let next = registry.open().unwrap();
        assert_eq!(registry.down(next, 0).unwrap().get(), 0);
    }

    #[test]
    fn source_ids_never_wrap_or_revive() {
        let mut registry = Registry {
            next: u64::MAX - 1,
            ..Registry::default()
        };
        let last = registry.open().unwrap();
        assert_eq!(last, Source(u64::MAX));
        registry.retire(last);
        let before = snapshot(&registry);
        assert_eq!(registry.open(), Err(Error::IdentityExhausted));
        assert_eq!(snapshot(&registry), before);
        assert_eq!(registry.down(last, 0), Err(Error::UnknownSource));
    }

    #[test]
    fn deterministic_interleavings_preserve_ownership_bounds_and_monotonic_sources() {
        let mut registry = Registry::default();
        let mut seen = BTreeSet::new();
        let mut last_closed = Source(0);
        let mut seed = 0x5eed_7011_u64;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let sources: Vec<_> = registry.sources.keys().copied().collect();
            let source = sources
                .get((seed >> 8) as usize % (sources.len() + 1))
                .copied()
                .unwrap_or(last_closed);
            let contact = ((seed >> 24) % 35) as u32;
            let physical = ((seed >> 40) % 68) as i32 - 2;
            match seed % 9 {
                0 => {
                    if let Ok(source) = registry.open() {
                        assert!(seen.insert(source));
                    }
                }
                1 => {
                    registry.down(source, contact).ok();
                }
                2 => {
                    registry.up(source, contact).ok();
                }
                3 => {
                    registry.motion(source, contact).ok();
                }
                4 => {
                    if let Some(retired) = registry.physical_down(physical) {
                        last_closed = retired.source;
                    }
                }
                5 => {
                    registry.physical_up(physical);
                }
                6 => {
                    registry.retire(source);
                    last_closed = source;
                }
                7 => {
                    registry.physical_cancel();
                }
                _ => {
                    registry.retire_all();
                }
            }
            invariants(&registry);
        }
    }
}
