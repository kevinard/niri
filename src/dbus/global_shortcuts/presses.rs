//! Captured key cycles retain their original recipient through modifier/rebind changes.
//! The Niri adapter calls begin only after accepting a new combined-seat press,
//! continue_cycle on raw source edges, and finish_key after seat reconciliation.
use std::collections::{BTreeMap, BTreeSet};

use super::registry::Target;

pub const MAX_CYCLES: usize = 512;
pub const MAX_HOLDERS: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub target: Target,
    pub activated: bool,
    pub timestamp_ms: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Update {
    pub captured: bool,
    pub edges: Vec<Edge>,
    pub overflow: Option<Target>,
}
impl Update {
    fn free() -> Self {
        Self {
            captured: false,
            edges: Vec::new(),
            overflow: None,
        }
    }
    fn captured(edges: Vec<Edge>) -> Self {
        Self {
            captured: true,
            edges,
            overflow: None,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Capacity,
}
struct Cycle {
    target: Option<Target>,
    holders: BTreeSet<u64>,
    untracked_holders: bool,
}
#[derive(Default)]
pub struct Presses {
    cycles: BTreeMap<u32, Cycle>,
}
fn edge(target: Target, activated: bool, timestamp_us: u64) -> Edge {
    Edge {
        target,
        activated,
        timestamp_ms: timestamp_us / 1000,
    }
}
impl Presses {
    pub fn keys(&self) -> Vec<u32> {
        self.cycles.keys().copied().collect()
    }
    fn active(&self, target: Target) -> bool {
        self.cycles
            .values()
            .any(|cycle| cycle.target == Some(target))
    }
    /// The caller must have checked that target is a current registration. An
    /// existing cycle always wins over a newly proposed recipient.
    pub fn begin(
        &mut self,
        source: u64,
        key: u32,
        target: Target,
        time_us: u64,
    ) -> Result<Update, Error> {
        if source == 0
            || key == 0
            || target.lease == 0
            || target.generation == 0
            || target.binding == 0
        {
            return Err(Error::Invalid);
        }
        if self.cycles.contains_key(&key) {
            return Ok(self.continue_cycle(source, key, true, time_us));
        }
        if self.cycles.len() == MAX_CYCLES {
            return Err(Error::Capacity);
        }
        let activated = !self.active(target);
        self.cycles.insert(
            key,
            Cycle {
                target: Some(target),
                holders: BTreeSet::from([source]),
                untracked_holders: false,
            },
        );
        Ok(Update::captured(if activated {
            vec![edge(target, true, time_us)]
        } else {
            Vec::new()
        }))
    }
    /// Captured cycles consume raw edges even if another source/modifier/layout
    /// now disagrees with the original shortcut. Seat-state updates still run.
    pub fn continue_cycle(&mut self, source: u64, key: u32, pressed: bool, time_us: u64) -> Update {
        let Some(cycle) = self.cycles.get_mut(&key) else {
            return Update::free();
        };
        if cycle.untracked_holders {
            return Update::captured(Vec::new());
        }
        if pressed {
            if cycle.holders.contains(&source) {
                return Update::captured(Vec::new());
            }
            if cycle.holders.len() == MAX_HOLDERS {
                let target = cycle.target;
                cycle.untracked_holders = true;
                cycle.holders.clear();
                let mut result = Update::captured(
                    target
                        .map(|t| self.retire([t], time_us))
                        .unwrap_or_default(),
                );
                result.overflow = target;
                return result;
            }
            cycle.holders.insert(source);
            return Update::captured(Vec::new());
        }
        if !cycle.holders.remove(&source) {
            return Update::captured(Vec::new());
        }
        if !cycle.holders.is_empty() {
            return Update::captured(Vec::new());
        }
        let target = cycle.target.take();
        // Keep a tombstone until the adapter proves the combined seat is up.
        // An unobserved source must not turn this into a forwarded stray release.
        let edges = target
            .filter(|t| !self.active(*t))
            .map(|t| vec![edge(t, false, time_us)])
            .unwrap_or_default();
        Update::captured(edges)
    }
    /// Called after input/source teardown with the actual combined-seat state.
    /// This also safely ends an overflow tombstone whose holders were bounded.
    pub fn finish_key(&mut self, key: u32, seat_down: bool, time_us: u64) -> Vec<Edge> {
        if seat_down {
            return Vec::new();
        }
        let Some(cycle) = self.cycles.remove(&key) else {
            return Vec::new();
        };
        cycle
            .target
            .filter(|t| !self.active(*t))
            .map(|t| vec![edge(t, false, time_us)])
            .unwrap_or_default()
    }
    /// Rebind/close retires authority but preserves suppression until held keys
    /// end. The same source cannot activate a replacement generation mid-hold.
    pub fn retire(&mut self, targets: impl IntoIterator<Item = Target>, time_us: u64) -> Vec<Edge> {
        let targets: BTreeSet<_> = targets.into_iter().collect();
        let mut active = BTreeSet::new();
        for cycle in self.cycles.values_mut() {
            if let Some(target) = cycle.target.filter(|t| targets.contains(t)) {
                active.insert(target);
                cycle.target = None;
            }
        }
        active
            .into_iter()
            .map(|t| edge(t, false, time_us))
            .collect()
    }
    pub fn remove_source(&mut self, source: u64, time_us: u64) -> Vec<Edge> {
        let keys: Vec<_> = self
            .cycles
            .iter()
            .filter(|(_, cycle)| cycle.holders.contains(&source))
            .map(|(&key, _)| key)
            .collect();
        keys.into_iter()
            .flat_map(|key| self.continue_cycle(source, key, false, time_us).edges)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(generation: u64) -> Target {
        Target {
            lease: 1,
            generation,
            binding: 2,
        }
    }
    #[test]
    fn release_follows_original_press_without_a_new_modifier_or_registry_lookup() {
        let mut p = Presses::default();
        let t = target(1);
        assert_eq!(
            p.begin(1, 38, t, 12000).unwrap().edges,
            vec![edge(t, true, 12000)]
        );
        assert_eq!(
            p.continue_cycle(1, 38, false, 18000).edges,
            vec![edge(t, false, 18000)]
        );
        assert!(p.finish_key(38, false, 18000).is_empty());
        assert!(!p.continue_cycle(1, 38, false, 19000).captured);
    }
    #[test]
    fn repeated_presses_and_nonowner_releases_never_add_or_end_an_activation() {
        let mut p = Presses::default();
        let t = target(1);
        p.begin(1, 38, t, 1000).unwrap();
        for _ in 0..100 {
            assert!(p.continue_cycle(1, 38, true, 2000).edges.is_empty());
        }
        assert!(p.continue_cycle(9, 38, false, 3000).edges.is_empty());
        assert_eq!(
            p.continue_cycle(1, 38, false, 4000).edges,
            vec![edge(t, false, 4000)]
        );
    }
    #[test]
    fn another_source_joins_the_captured_cycle_and_last_release_ends_it() {
        let mut p = Presses::default();
        let t = target(1);
        p.begin(1, 38, t, 1000).unwrap();
        assert!(p.continue_cycle(2, 38, true, 2000).edges.is_empty());
        assert!(p.continue_cycle(1, 38, false, 3000).edges.is_empty());
        assert!(p.finish_key(38, true, 3000).is_empty());
        assert_eq!(
            p.continue_cycle(2, 38, false, 4000).edges,
            vec![edge(t, false, 4000)]
        );
    }
    #[test]
    fn different_key_aliases_share_one_action_activation() {
        let mut p = Presses::default();
        let t = target(1);
        p.begin(1, 38, t, 1000).unwrap();
        assert!(p.begin(2, 52, t, 2000).unwrap().edges.is_empty());
        assert!(p.continue_cycle(1, 38, false, 3000).edges.is_empty());
        assert_eq!(
            p.continue_cycle(2, 52, false, 4000).edges,
            vec![edge(t, false, 4000)]
        );
    }
    #[test]
    fn rebind_retires_once_and_cannot_recapture_an_unchanged_held_key() {
        let mut p = Presses::default();
        let old = target(1);
        let new = target(2);
        p.begin(1, 38, old, 1000).unwrap();
        assert_eq!(p.retire([old], 2000), vec![edge(old, false, 2000)]);
        assert!(p.begin(1, 38, new, 3000).unwrap().edges.is_empty());
        assert!(p.begin(2, 38, new, 4000).unwrap().edges.is_empty());
        assert!(p.continue_cycle(1, 38, false, 5000).edges.is_empty());
        assert!(p.continue_cycle(2, 38, false, 6000).edges.is_empty());
        p.finish_key(38, false, 6000);
        assert_eq!(
            p.begin(1, 38, new, 7000).unwrap().edges,
            vec![edge(new, true, 7000)]
        );
    }
    #[test]
    fn source_loss_retires_only_the_last_held_recipient() {
        let mut p = Presses::default();
        let t = target(1);
        p.begin(1, 38, t, 1000).unwrap();
        p.continue_cycle(2, 38, true, 2000);
        assert!(p.remove_source(1, 3000).is_empty());
        assert_eq!(p.remove_source(2, 4000), vec![edge(t, false, 4000)]);
        assert!(p.remove_source(2, 5000).is_empty());
    }
    #[test]
    fn timestamps_preserve_source_milliseconds_past_the_u32_wrap() {
        let mut p = Presses::default();
        let t = target(1);
        let micros = (u64::from(u32::MAX) + 15) * 1000 + 999;
        assert_eq!(
            p.begin(1, 38, t, micros).unwrap().edges[0].timestamp_ms,
            u64::from(u32::MAX) + 15
        );
        // Input time is not required to increase; do not replace it with receipt time.
        assert_eq!(
            p.continue_cycle(1, 38, false, 1234).edges[0].timestamp_ms,
            1
        );
    }
    #[test]
    fn holder_overflow_retires_authority_and_preserves_a_bounded_tombstone() {
        let mut p = Presses::default();
        let t = target(1);
        p.begin(1, 38, t, 1000).unwrap();
        for source in 2..=MAX_HOLDERS as u64 {
            p.continue_cycle(source, 38, true, 2000);
        }
        let update = p.continue_cycle(1000, 38, true, 3000);
        assert_eq!(update.overflow, Some(t));
        assert_eq!(update.edges, vec![edge(t, false, 3000)]);
        assert_eq!(p.cycles[&38].holders.len(), 0);
        for source in 1001..2000 {
            assert!(p.continue_cycle(source, 38, true, 4000).edges.is_empty());
        }
        assert!(p.finish_key(38, true, 5000).is_empty());
        assert!(p.continue_cycle(1000, 38, false, 6000).captured);
        assert!(p.finish_key(38, false, 7000).is_empty());
        assert_eq!(
            p.begin(1, 38, target(2), 8000).unwrap().edges,
            vec![edge(target(2), true, 8000)]
        );
    }
    #[test]
    fn finite_cycle_budget_refuses_a_new_press_without_changing_existing_owners() {
        let mut p = Presses::default();
        let t = target(1);
        for key in 1..=MAX_CYCLES as u32 {
            p.begin(1, key, t, 1000).unwrap();
        }
        assert_eq!(p.begin(1, 9000, t, 2000), Err(Error::Capacity));
        assert!(!p.continue_cycle(1, 9000, false, 3000).captured);
        assert_eq!(p.retire([t], 4000), vec![edge(t, false, 4000)]);
    }
}
