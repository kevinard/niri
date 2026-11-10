//! Bounded, atomic shortcut registration after compositor keymap resolution.
//!
//! This source candidate is not wired into Niri yet. The adapter resolves XDG
//! keysyms to base-layer keycodes, reserves ordinary/a11y binds, and retires press
//! ownership for every target returned by a commit or close.
use std::collections::{BTreeMap, BTreeSet};

pub const CTRL: u8 = 1;
pub const ALT: u8 = 1 << 1;
pub const SHIFT: u8 = 1 << 2;
pub const NUM: u8 = 1 << 3;
pub const LOGO: u8 = 1 << 4;
pub const MODIFIERS: u8 = CTRL | ALT | SHIFT | NUM | LOGO;
pub const MAX_LEASES: usize = 16;
pub const MAX_BINDINGS: usize = 64;
pub const MAX_KEYCODES_PER_BINDING: usize = 16;
pub const MAX_KEYCODES_PER_LEASE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot {
    pub code: u32,
    pub layout: u32,
}
impl Slot {
    pub fn new(code: u32, layout: u32) -> Self {
        Self { code, layout }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub id: u32,
    pub keys: Vec<Slot>,
    pub modifiers: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub lease: u64,
    pub generation: u64,
    pub binding: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    Reserved { binding: u32 },
    Occupied { binding: u32, owner: Target },
    DuplicateTrigger { binding: u32, other: u32 },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    UnknownLease,
    Limits,
    InvalidBinding,
    Exhausted,
    Conflicts(Vec<Conflict>),
}
#[derive(Debug, PartialEq, Eq)]
pub struct Commit {
    pub generation: u64,
    pub retired: Vec<Target>,
}
#[derive(Default)]
struct Lease {
    generation: u64,
    bindings: Vec<Binding>,
}
pub struct Registry {
    next_lease: u64,
    leases: BTreeMap<u64, Lease>,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            next_lease: 1,
            leases: BTreeMap::new(),
        }
    }
}
fn overlaps(left: &Binding, right: &Binding) -> bool {
    // Ordinary shortcuts ignore Num Lock. An explicit NUM additionally requires
    // it to be active, so either form overlaps the other while Num Lock is on.
    left.modifiers & !NUM == right.modifiers & !NUM
        && left
            .keys
            .iter()
            .any(|code| right.keys.binary_search(code).is_ok())
}
fn targets(id: u64, lease: &Lease) -> Vec<Target> {
    lease
        .bindings
        .iter()
        .map(|b| Target {
            lease: id,
            generation: lease.generation,
            binding: b.id,
        })
        .collect()
}
impl Registry {
    pub fn open(&mut self) -> Result<u64, Error> {
        if self.leases.len() >= MAX_LEASES {
            return Err(Error::Limits);
        }
        let id = self.next_lease;
        let next = id.checked_add(1).ok_or(Error::Exhausted)?;
        self.leases.insert(id, Lease::default());
        self.next_lease = next;
        Ok(id)
    }
    /// Replace one lease's complete set or leave all registration state intact.
    /// `reserved` includes configured/hardcoded/a11y conflicts in the current
    /// compositor keymap; the registry additionally checks every other lease.
    pub fn replace(
        &mut self,
        id: u64,
        mut bindings: Vec<Binding>,
        mut reserved: impl FnMut(Slot, u8) -> bool,
    ) -> Result<Commit, Error> {
        let previous = self.leases.get(&id).ok_or(Error::UnknownLease)?;
        let generation = previous.generation.checked_add(1).ok_or(Error::Exhausted)?;
        if bindings.len() > MAX_BINDINGS {
            return Err(Error::Limits);
        }
        let mut ids = BTreeSet::new();
        let mut total = 0;
        for binding in &mut bindings {
            if binding.id == 0
                || !ids.insert(binding.id)
                || binding.modifiers & !MODIFIERS != 0
                || binding.keys.is_empty()
                || binding.keys.iter().any(|slot| slot.code == 0)
            {
                return Err(Error::InvalidBinding);
            }
            if binding.keys.len() > MAX_KEYCODES_PER_BINDING {
                return Err(Error::Limits);
            }
            total += binding.keys.len();
            if total > MAX_KEYCODES_PER_LEASE {
                return Err(Error::Limits);
            }
            binding.keys.sort_unstable();
            if binding.keys.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(Error::InvalidBinding);
            }
        }
        let mut conflicts = Vec::new();
        // One actionable reason per binding keeps conflict replies bounded too.
        'candidate: for (index, binding) in bindings.iter().enumerate() {
            if binding
                .keys
                .iter()
                .any(|&code| reserved(code, binding.modifiers))
            {
                conflicts.push(Conflict::Reserved {
                    binding: binding.id,
                });
                continue;
            }
            for other in &bindings[..index] {
                if overlaps(binding, other) {
                    conflicts.push(Conflict::DuplicateTrigger {
                        binding: binding.id,
                        other: other.id,
                    });
                    continue 'candidate;
                }
            }
            for (&other_id, lease) in &self.leases {
                if other_id == id {
                    continue;
                }
                for other in &lease.bindings {
                    if overlaps(binding, other) {
                        conflicts.push(Conflict::Occupied {
                            binding: binding.id,
                            owner: Target {
                                lease: other_id,
                                generation: lease.generation,
                                binding: other.id,
                            },
                        });
                        continue 'candidate;
                    }
                }
            }
        }
        if !conflicts.is_empty() {
            return Err(Error::Conflicts(conflicts));
        }
        let retired = targets(id, previous);
        self.leases.insert(
            id,
            Lease {
                generation,
                bindings,
            },
        );
        Ok(Commit {
            generation,
            retired,
        })
    }
    pub fn all_targets(&self) -> Vec<Target> {
        self.leases
            .iter()
            .flat_map(|(&id, lease)| targets(id, lease))
            .collect()
    }
    pub fn close(&mut self, id: u64) -> Option<Vec<Target>> {
        self.leases.remove(&id).map(|lease| targets(id, &lease))
    }
    pub fn lookup(&self, key: Slot, modifiers: u8) -> Option<Target> {
        // Unexpected XKB modifiers are not silently treated as an ordinary
        // CTRL/ALT/etc combination. The adapter derives this mask by name.
        if modifiers & !MODIFIERS != 0 {
            return None;
        }
        self.leases.iter().find_map(|(&id, lease)| {
            lease
                .bindings
                .iter()
                .find(|binding| {
                    binding.keys.binary_search(&key).is_ok()
                        && binding.modifiers & !NUM == modifiers & !NUM
                        && (binding.modifiers & NUM == 0 || modifiers & NUM != 0)
                })
                .map(|binding| Target {
                    lease: id,
                    generation: lease.generation,
                    binding: binding.id,
                })
        })
    }
    #[cfg(test)]
    pub fn is_current(&self, target: Target) -> bool {
        self.leases.get(&target.lease).is_some_and(|lease| {
            lease.generation == target.generation
                && lease.bindings.iter().any(|b| b.id == target.binding)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(id: u32, codes: &[u32], modifiers: u8) -> Binding {
        Binding {
            id,
            keys: codes.iter().map(|&code| Slot::new(code, 0)).collect(),
            modifiers,
        }
    }
    fn commit(registry: &mut Registry, lease: u64, bindings: Vec<Binding>) -> Commit {
        registry.replace(lease, bindings, |_, _| false).unwrap()
    }
    #[test]
    fn keymap_aliases_cannot_steal_another_leases_trigger() {
        let mut r = Registry::default();
        let first = r.open().unwrap();
        let second = r.open().unwrap();
        commit(&mut r, first, vec![binding(1, &[38, 52], CTRL)]);
        let original = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        let error = r
            .replace(second, vec![binding(8, &[52, 77], CTRL)], |_, _| false)
            .unwrap_err();
        assert_eq!(
            error,
            Error::Conflicts(vec![Conflict::Occupied {
                binding: 8,
                owner: original
            }])
        );
        assert_eq!(r.lookup(Slot::new(52, 0), CTRL), Some(original));
        assert_eq!(r.lookup(Slot::new(77, 0), CTRL), None);
    }
    #[test]
    fn a_valid_prefix_never_partially_commits_after_any_conflict() {
        let mut r = Registry::default();
        let lease = r.open().unwrap();
        commit(&mut r, lease, vec![binding(1, &[38], CTRL)]);
        let original = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        assert_eq!(
            r.replace(
                lease,
                vec![binding(2, &[40], ALT), binding(3, &[41], ALT)],
                |code, _| code.code == 41
            ),
            Err(Error::Conflicts(vec![Conflict::Reserved { binding: 3 }]))
        );
        assert_eq!(r.lookup(Slot::new(38, 0), CTRL), Some(original));
        assert!(r.is_current(original));
        assert_eq!(r.lookup(Slot::new(40, 0), ALT), None);
    }
    #[test]
    fn rebind_retires_old_generation_even_when_action_id_is_reused() {
        let mut r = Registry::default();
        let lease = r.open().unwrap();
        commit(&mut r, lease, vec![binding(1, &[38], CTRL)]);
        let old = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        let changed = commit(&mut r, lease, vec![binding(1, &[40], ALT)]);
        assert_eq!(changed.retired, vec![old]);
        assert!(!r.is_current(old));
        assert_eq!(r.lookup(Slot::new(38, 0), CTRL), None);
        let new = r.lookup(Slot::new(40, 0), ALT).unwrap();
        assert_eq!(new.generation, changed.generation);
        assert!(r.is_current(new));
        assert_eq!(r.close(lease), Some(vec![new]));
        assert_eq!(r.close(lease), None);
        assert!(!r.is_current(new));
        let replacement = r.open().unwrap();
        assert_ne!(replacement, lease);
    }
    #[test]
    fn num_lock_is_optional_unless_requested_and_conflicts_are_honest() {
        let mut r = Registry::default();
        let a = r.open().unwrap();
        let b = r.open().unwrap();
        commit(&mut r, a, vec![binding(1, &[38], CTRL | NUM)]);
        assert_eq!(r.lookup(Slot::new(38, 0), CTRL), None);
        assert!(r.lookup(Slot::new(38, 0), CTRL | NUM).is_some());
        assert!(matches!(
            r.replace(b, vec![binding(2, &[38], CTRL)], |_, _| false),
            Err(Error::Conflicts(_))
        ));
        r.close(a);
        commit(&mut r, b, vec![binding(2, &[38], CTRL)]);
        assert_eq!(
            r.lookup(Slot::new(38, 0), CTRL),
            r.lookup(Slot::new(38, 0), CTRL | NUM)
        );
        assert_eq!(r.lookup(Slot::new(38, 0), CTRL | 128), None);
    }
    #[test]
    fn invalid_and_ambiguous_sets_preserve_existing_authority() {
        let mut r = Registry::default();
        let lease = r.open().unwrap();
        commit(&mut r, lease, vec![binding(1, &[38], CTRL)]);
        let old = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        for proposed in [
            vec![binding(0, &[40], ALT)],
            vec![binding(2, &[], ALT)],
            vec![binding(2, &[0], ALT)],
            vec![binding(2, &[40, 40], ALT)],
            vec![binding(2, &[40], 128)],
            vec![binding(2, &[40], ALT), binding(2, &[41], ALT)],
        ] {
            assert_eq!(
                r.replace(lease, proposed, |_, _| false),
                Err(Error::InvalidBinding)
            );
            assert!(r.is_current(old));
        }
        assert!(matches!(
            r.replace(
                lease,
                vec![binding(2, &[40], ALT), binding(3, &[40], ALT | NUM)],
                |_, _| false
            ),
            Err(Error::Conflicts(_))
        ));
        assert!(r.is_current(old));
    }
    #[test]
    fn finite_admission_and_generation_exhaustion_never_reset_state() {
        let mut r = Registry::default();
        let lease = r.open().unwrap();
        commit(&mut r, lease, vec![binding(1, &[38], CTRL)]);
        let old = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        assert_eq!(
            r.replace(
                lease,
                (1..=65).map(|id| binding(id, &[id + 100], ALT)).collect(),
                |_, _| false
            ),
            Err(Error::Limits)
        );
        assert!(r.is_current(old));
        for _ in 1..MAX_LEASES {
            r.open().unwrap();
        }
        assert_eq!(r.open(), Err(Error::Limits));
        r.close(2);
        r.open().unwrap();
        assert!(r.is_current(old));
        r.leases.get_mut(&lease).unwrap().generation = u64::MAX;
        assert_eq!(
            r.replace(lease, vec![], |_, _| false),
            Err(Error::Exhausted)
        );
        assert!(r.lookup(Slot::new(38, 0), CTRL).is_some());
        r.close(lease);
        r.next_lease = u64::MAX;
        assert_eq!(r.open(), Err(Error::Exhausted));
    }
    #[test]
    fn clearing_one_lease_retires_only_its_targets_and_frees_its_reservations() {
        let mut r = Registry::default();
        let a = r.open().unwrap();
        let b = r.open().unwrap();
        commit(&mut r, a, vec![binding(1, &[38], CTRL)]);
        commit(&mut r, b, vec![binding(2, &[40], ALT)]);
        let first = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        let second = r.lookup(Slot::new(40, 0), ALT).unwrap();
        assert_eq!(commit(&mut r, a, vec![]).retired, vec![first]);
        assert!(r.is_current(second));
        commit(&mut r, a, vec![binding(3, &[38], CTRL)]);
        assert_ne!(r.lookup(Slot::new(38, 0), CTRL), Some(first));
        assert!(r.is_current(second));
    }
    #[test]
    fn conflicting_batch_reports_each_action_once_without_amplifying_the_reply() {
        let mut r = Registry::default();
        let first = r.open().unwrap();
        let second = r.open().unwrap();
        commit(&mut r, first, vec![binding(9, &[38], CTRL)]);
        let original = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        let proposed = (1..=MAX_BINDINGS as u32)
            .map(|id| binding(id, &[38], CTRL))
            .collect();
        let Err(Error::Conflicts(conflicts)) = r.replace(second, proposed, |_, _| false) else {
            panic!("Conflicting batch was accepted");
        };
        assert_eq!(conflicts.len(), MAX_BINDINGS);
        let reported: BTreeSet<_> = conflicts
            .into_iter()
            .map(|reason| match reason {
                Conflict::Reserved { binding }
                | Conflict::Occupied { binding, .. }
                | Conflict::DuplicateTrigger { binding, .. } => binding,
            })
            .collect();
        assert_eq!(reported.len(), MAX_BINDINGS);
        assert_eq!(r.lookup(Slot::new(38, 0), CTRL), Some(original));
    }
    #[test]
    fn keymap_resolution_budget_is_checked_before_any_registration_changes() {
        let mut r = Registry::default();
        let lease = r.open().unwrap();
        commit(&mut r, lease, vec![binding(1, &[38], CTRL)]);
        let old = r.lookup(Slot::new(38, 0), CTRL).unwrap();
        let too_many_codes: Vec<_> = (1..=17).collect();
        assert_eq!(
            r.replace(lease, vec![binding(2, &too_many_codes, ALT)], |_, _| false),
            Err(Error::Limits)
        );
        let too_many_total = (1..=33)
            .map(|id| {
                binding(
                    id,
                    &(0..16)
                        .map(|code| 1000 + id * 16 + code)
                        .collect::<Vec<_>>(),
                    ALT,
                )
            })
            .collect();
        assert_eq!(
            r.replace(lease, too_many_total, |_, _| false),
            Err(Error::Limits)
        );
        assert!(r.is_current(old));
        assert_eq!(
            r.replace(99, vec![], |_, _| false),
            Err(Error::UnknownLease)
        );
    }
}
