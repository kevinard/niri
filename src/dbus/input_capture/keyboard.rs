//! Source-owned keyboard routing for compositor capture. Captured keys never
//! enter the ordinary seat; pre-existing local presses keep their local release.
//! Deactivation lifts EI keys but suppresses their physical tails until seat-up.
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_SOURCES: usize = 256;
pub const MAX_PRESSES: usize = 4096;
pub const MAX_KEY: u32 = 0x2ff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recipient {
    pub lease: u64,
    pub activation: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: u32,
    pub pressed: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Local,
    Intercept,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub route: Route,
    /// A first-down/final-up for the separate capture XKB state. Retired keys
    /// are already lifted during deactivation and have no second XKB release.
    /// Local seat XKB must be updated only through Route::Local.
    pub physical: Option<Key>,
    pub captured: Option<(Recipient, Key)>,
}
impl Outcome {
    fn local() -> Self {
        Self {
            route: Route::Local,
            physical: None,
            captured: None,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Retirement {
    pub recipient: Recipient,
    /// Send these releases before stopping EI emulation, including on owner loss.
    pub releases: Vec<Key>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid,
    Active,
    Exhausted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Local,
    Capture(Recipient),
    Suppressed,
}
struct Held {
    sources: BTreeSet<u64>,
    owner: Owner,
}
#[derive(Default)]
pub struct Keyboard {
    keys: BTreeMap<u32, Held>,
    sources: BTreeMap<u64, usize>,
    presses: usize,
    active: Option<Recipient>,
    exhausted: bool,
    emergency: Option<u64>,
}
impl Keyboard {
    pub fn active(&self) -> Option<Recipient> {
        self.active
    }
    pub fn has_suppressed_source(&self, source: u64) -> bool {
        self.keys
            .values()
            .any(|held| held.owner == Owner::Suppressed && held.sources.contains(&source))
            || self.emergency == Some(source)
    }
    pub fn admissible(&self) -> bool {
        !self.exhausted
    }
    pub fn close_admission(&mut self) {
        self.exhausted = true;
    }
    pub fn activate(&mut self, recipient: Recipient) -> Result<(), Refusal> {
        if recipient.lease == 0 || recipient.activation == 0 {
            return Err(Refusal::Invalid);
        }
        if self.exhausted {
            return Err(Refusal::Exhausted);
        }
        if self.active.is_some() {
            return Err(Refusal::Active);
        }
        self.active = Some(recipient);
        Ok(())
    }
    /// Seed a new capture XKB state from eligible, already local keys. Keys held
    /// over from a retired capture cannot transfer modifiers to a new recipient.
    pub fn local_keys(&self) -> impl Iterator<Item = u32> + '_ {
        self.keys
            .iter()
            .filter_map(|(&code, held)| (held.owner == Owner::Local).then_some(code))
    }
    pub fn deactivate(&mut self) -> Option<Retirement> {
        let recipient = self.active.take()?;
        let mut releases = Vec::new();
        for (&code, held) in &mut self.keys {
            if held.owner == Owner::Capture(recipient) {
                held.owner = Owner::Suppressed;
                releases.push(Key {
                    code,
                    pressed: false,
                });
            }
        }
        Some(Retirement {
            recipient,
            releases,
        })
    }
    /// Called for every raw eligible-source edge, before ordinary seat handling.
    /// A tracked release remains owned even when this source is no longer
    /// eligible. Excluded injection sources otherwise pass straight through.
    /// A source excluded at press time must stay excluded through that press;
    /// admission cannot change the identity/eligibility of a held source.
    ///
    /// On refusal the caller must deactivate capture and process this untracked
    /// edge locally. Exhaustion permanently closes admission: untracked held
    /// input must not later be reinterpreted as a fresh capture press.
    pub fn edge(
        &mut self,
        source: u64,
        code: u32,
        pressed: bool,
        eligible: bool,
    ) -> Result<Outcome, Refusal> {
        self.transition(
            source,
            code,
            pressed,
            eligible.then_some(self.active.map_or(Owner::Local, Owner::Capture)),
        )
    }
    /// Reserve the emergency pause key after deactivation. It is never an EI
    /// press and its physical key-up must not leak to the local application.
    pub fn suppress_press(&mut self, source: u64, code: u32) -> Result<Outcome, Refusal> {
        if code != 1 {
            return Err(Refusal::Invalid);
        }
        match self.transition(source, code, true, Some(Owner::Suppressed)) {
            Err(Refusal::Exhausted)
                if self.emergency.is_none() || self.emergency == Some(source) =>
            {
                // One fixed emergency tombstone remains available after normal
                // admission fills. Exhaustion already prevents another capture.
                self.emergency = Some(source);
                Ok(Outcome {
                    route: Route::Intercept,
                    physical: None,
                    captured: None,
                })
            }
            result => result,
        }
    }
    fn transition(
        &mut self,
        source: u64,
        code: u32,
        pressed: bool,
        owner: Option<Owner>,
    ) -> Result<Outcome, Refusal> {
        if source == 0 || !(1..=MAX_KEY).contains(&code) {
            return Err(Refusal::Invalid);
        }
        if code == 1 && self.emergency == Some(source) {
            if !pressed {
                self.emergency = None;
            }
            return Ok(Outcome {
                route: Route::Intercept,
                physical: None,
                captured: None,
            });
        }
        let known = self
            .keys
            .get(&code)
            .is_some_and(|held| held.sources.contains(&source));
        if !pressed {
            return Ok(if known {
                self.release(source, code)
            } else {
                Outcome::local()
            });
        }
        if known {
            let owner = self.keys[&code].owner;
            return Ok(Outcome {
                route: if owner == Owner::Local {
                    Route::Local
                } else {
                    Route::Intercept
                },
                physical: None,
                captured: None,
            });
        }
        let Some(owner) = owner else {
            return Ok(Outcome::local());
        };
        if self.exhausted
            || self.presses == MAX_PRESSES
            || (!self.sources.contains_key(&source) && self.sources.len() == MAX_SOURCES)
        {
            self.exhausted = true;
            return Err(Refusal::Exhausted);
        }
        self.presses += 1;
        *self.sources.entry(source).or_default() += 1;
        let first = !self.keys.contains_key(&code);
        let held = self.keys.entry(code).or_insert_with(|| Held {
            sources: BTreeSet::new(),
            owner,
        });
        held.sources.insert(source);
        Ok(Self::outcome(
            held.owner,
            Key {
                code,
                pressed: true,
            },
            first,
        ))
    }
    fn outcome(owner: Owner, key: Key, transition: bool) -> Outcome {
        Outcome {
            route: if owner == Owner::Local {
                Route::Local
            } else {
                Route::Intercept
            },
            physical: (transition && owner != Owner::Suppressed).then_some(key),
            captured: match owner {
                Owner::Capture(recipient) if transition => Some((recipient, key)),
                _ => None,
            },
        }
    }
    fn release(&mut self, source: u64, code: u32) -> Outcome {
        let held = self.keys.get_mut(&code).expect("tracked source press");
        assert!(held.sources.remove(&source));
        self.presses -= 1;
        let count = self.sources.get_mut(&source).expect("tracked source count");
        *count -= 1;
        if *count == 0 {
            self.sources.remove(&source);
        }
        let owner = held.owner;
        let last = held.sources.is_empty();
        if last {
            self.keys.remove(&code);
        }
        Self::outcome(
            owner,
            Key {
                code,
                pressed: false,
            },
            last,
        )
    }
    /// Pair EI releases for a removed source. Local releases are performed by
    /// the ordinary seat's existing release_source path, not injected twice.
    pub fn source_closed(&mut self, source: u64) -> Vec<Outcome> {
        let emergency = self.emergency == Some(source);
        if emergency {
            self.emergency = None;
        }
        let keys: Vec<_> = self
            .keys
            .iter()
            .filter_map(|(&code, held)| held.sources.contains(&source).then_some(code))
            .collect();
        let mut outcomes: Vec<_> = keys
            .into_iter()
            .map(|code| self.release(source, code))
            .collect();
        if emergency {
            outcomes.push(Outcome {
                route: Route::Intercept,
                physical: None,
                captured: None,
            });
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recipient(lease: u64, activation: u32) -> Recipient {
        Recipient { lease, activation }
    }
    fn key(code: u32, pressed: bool) -> Key {
        Key { code, pressed }
    }
    #[test]
    fn emergency_escape_remains_paired_after_normal_source_admission_is_full() {
        let mut k = Keyboard::default();
        k.activate(recipient(1, 1)).unwrap();
        for source in 1..=MAX_SOURCES as u64 {
            edge(&mut k, source, 30, true);
        }
        k.deactivate();
        let extra = MAX_SOURCES as u64 + 1;
        assert_eq!(k.suppress_press(extra, 1).unwrap().route, Route::Intercept);
        assert!(!k.admissible());
        assert_eq!(edge(&mut k, extra, 1, false).route, Route::Intercept);
        assert_eq!(k.activate(recipient(2, 1)), Err(Refusal::Exhausted));
    }
    #[test]
    fn source_identity_exhaustion_closes_admission_without_forgetting_old_capture_tails() {
        let mut k = Keyboard::default();
        assert!(k.admissible());
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        captured(edge(&mut k, 1, 30, true), owner, 30, true);
        k.close_admission();
        assert_eq!(k.active(), Some(owner));
        assert_eq!(k.deactivate().unwrap().releases, vec![key(30, false)]);
        assert_eq!(edge(&mut k, 1, 30, false).route, Route::Intercept);
        assert_eq!(k.activate(recipient(2, 1)), Err(Refusal::Exhausted));
    }
    fn edge(k: &mut Keyboard, source: u64, code: u32, pressed: bool) -> Outcome {
        k.edge(source, code, pressed, true).unwrap()
    }
    fn captured(outcome: Outcome, owner: Recipient, code: u32, pressed: bool) {
        assert_eq!(outcome.route, Route::Intercept);
        assert_eq!(outcome.captured, Some((owner, key(code, pressed))));
    }
    #[test]
    fn preexisting_local_modifiers_keep_their_local_release_while_new_keys_are_captured() {
        let mut k = Keyboard::default();
        let owner = recipient(1, 1);
        assert_eq!(edge(&mut k, 1, 29, true).route, Route::Local);
        assert_eq!(k.local_keys().collect::<Vec<_>>(), vec![29]);
        k.activate(owner).unwrap();
        captured(edge(&mut k, 1, 30, true), owner, 30, true);
        let release = edge(&mut k, 1, 29, false);
        assert_eq!(release.route, Route::Local);
        assert_eq!(release.captured, None);
        assert_eq!(release.physical, Some(key(29, false)));
        assert_eq!(
            k.deactivate(),
            Some(Retirement {
                recipient: owner,
                releases: vec![key(30, false)]
            })
        );
        assert_eq!(edge(&mut k, 1, 30, false).route, Route::Intercept);
        assert_eq!(edge(&mut k, 1, 30, true).route, Route::Local);
    }
    #[test]
    fn overlapping_sources_and_repeats_produce_one_captured_press_and_final_release() {
        let mut k = Keyboard::default();
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        captured(edge(&mut k, 1, 30, true), owner, 30, true);
        for source in [1, 2, 2] {
            let repeat = edge(&mut k, source, 30, true);
            assert_eq!(repeat.route, Route::Intercept);
            assert_eq!(repeat.captured, None);
            assert_eq!(repeat.physical, None);
        }
        let first = k.source_closed(1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].captured, None);
        assert_eq!(edge(&mut k, 1, 30, false).captured, None);
        let final_source = k.source_closed(2);
        assert_eq!(final_source.len(), 1);
        captured(final_source[0], owner, 30, false);
        assert!(k.deactivate().unwrap().releases.is_empty());
    }
    #[test]
    fn retired_held_keys_cannot_move_to_another_activation_or_lease() {
        let mut k = Keyboard::default();
        let old = recipient(7, 1);
        k.activate(old).unwrap();
        captured(edge(&mut k, 1, 42, true), old, 42, true);
        assert_eq!(k.deactivate().unwrap().releases, vec![key(42, false)]);
        assert!(k.has_suppressed_source(1));
        let new = recipient(8, 1);
        k.activate(new).unwrap();
        assert!(k.local_keys().next().is_none());
        assert_eq!(edge(&mut k, 2, 42, true).captured, None);
        assert_eq!(edge(&mut k, 1, 42, false).route, Route::Intercept);
        let tail = k.edge(2, 42, false, false).unwrap();
        assert_eq!(tail.route, Route::Intercept);
        assert_eq!(tail.captured, None);
        assert_eq!(tail.physical, None, "retirement already lifted this key");
        assert!(!k.has_suppressed_source(1));
        assert!(!k.has_suppressed_source(2));
        captured(edge(&mut k, 2, 42, true), new, 42, true);
        assert_eq!(k.deactivate().unwrap().recipient, new);
    }
    #[test]
    fn a_second_source_does_not_capture_a_key_already_held_locally() {
        let mut k = Keyboard::default();
        edge(&mut k, 1, 30, true);
        k.activate(recipient(1, 1)).unwrap();
        for (source, pressed) in [(2, true), (1, false), (2, false)] {
            let event = edge(&mut k, source, 30, pressed);
            assert_eq!(event.route, Route::Local);
            assert_eq!(event.captured, None);
        }
        captured(edge(&mut k, 2, 30, true), recipient(1, 1), 30, true);
    }
    #[test]
    fn excluded_injected_keys_do_not_enter_capture_or_release_another_sources_key() {
        let mut k = Keyboard::default();
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        captured(edge(&mut k, 1, 30, true), owner, 30, true);
        for pressed in [true, false] {
            let injected = k.edge(2, 30, pressed, false).unwrap();
            assert_eq!(injected, Outcome::local());
        }
        assert!(k.source_closed(2).is_empty());
        captured(edge(&mut k, 1, 30, false), owner, 30, false);
    }
    #[test]
    fn all_held_keys_are_released_before_emulation_stops_and_their_tails_stay_suppressed() {
        let mut k = Keyboard::default();
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        for code in [56, 29, 30] {
            captured(edge(&mut k, 1, code, true), owner, code, true);
        }
        assert_eq!(
            k.deactivate(),
            Some(Retirement {
                recipient: owner,
                releases: vec![key(29, false), key(30, false), key(56, false)]
            })
        );
        assert!(k.deactivate().is_none());
        for event in k.source_closed(1) {
            assert_eq!(event.route, Route::Intercept);
            assert_eq!(event.captured, None);
        }
        assert_eq!(edge(&mut k, 1, 30, true).route, Route::Local);
    }
    #[test]
    fn emergency_pause_lifts_captured_modifiers_without_sending_its_escape_to_either_client() {
        let mut k = Keyboard::default();
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        for code in [29, 56] {
            captured(edge(&mut k, 1, code, true), owner, code, true);
        }
        assert_eq!(
            k.deactivate().unwrap().releases,
            vec![key(29, false), key(56, false)]
        );
        let escape = k.suppress_press(1, 1).unwrap();
        assert_eq!(
            escape,
            Outcome {
                route: Route::Intercept,
                physical: None,
                captured: None
            }
        );
        k.activate(recipient(2, 1)).unwrap();
        assert!(k.local_keys().next().is_none());
        for code in [1, 29, 56] {
            let tail = edge(&mut k, 1, code, false);
            assert_eq!(
                tail,
                Outcome {
                    route: Route::Intercept,
                    physical: None,
                    captured: None
                }
            );
        }
        captured(edge(&mut k, 1, 1, true), recipient(2, 1), 1, true);
    }
    #[test]
    fn source_limit_refuses_new_authority_without_forgetting_owned_release_tails() {
        let mut k = Keyboard::default();
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        for source in 1..=MAX_SOURCES as u64 {
            edge(&mut k, source, 30, true);
        }
        assert_eq!(
            k.edge(MAX_SOURCES as u64 + 1, 30, true, true),
            Err(Refusal::Exhausted)
        );
        assert_eq!(k.deactivate().unwrap().releases, vec![key(30, false)]);
        for source in 1..=MAX_SOURCES as u64 {
            let event = edge(&mut k, source, 30, false);
            assert_eq!(event.route, Route::Intercept);
            assert_eq!(event.captured, None);
        }
        assert_eq!(k.activate(recipient(2, 1)), Err(Refusal::Exhausted));
        assert_eq!(k.edge(1, 31, true, false), Ok(Outcome::local()));
    }
    #[test]
    fn press_limit_preserves_bounded_pairing_and_permanently_refuses_readmission() {
        let mut k = Keyboard::default();
        k.activate(recipient(1, 1)).unwrap();
        let mut accepted = 0;
        'fill: for source in 1..=MAX_SOURCES as u64 {
            for code in 1..=MAX_KEY {
                match k.edge(source, code, true, true) {
                    Ok(_) => accepted += 1,
                    Err(Refusal::Exhausted) => break 'fill,
                    other => panic!("unexpected admission: {other:?}"),
                }
            }
        }
        assert_eq!(accepted, MAX_PRESSES);
        assert_eq!(k.deactivate().unwrap().releases.len(), MAX_KEY as usize);
        for source in 1..=MAX_SOURCES as u64 {
            k.source_closed(source);
        }
        assert_eq!(k.activate(recipient(2, 1)), Err(Refusal::Exhausted));
        assert!(k.local_keys().next().is_none());
    }
    #[test]
    fn invalid_or_overlapping_activation_cannot_replace_the_current_recipient() {
        let mut k = Keyboard::default();
        assert_eq!(k.activate(recipient(0, 1)), Err(Refusal::Invalid));
        assert_eq!(k.activate(recipient(1, 0)), Err(Refusal::Invalid));
        let owner = recipient(1, 1);
        k.activate(owner).unwrap();
        assert_eq!(k.activate(recipient(2, 1)), Err(Refusal::Active));
        assert_eq!(k.edge(0, 30, true, true), Err(Refusal::Invalid));
        assert_eq!(k.edge(1, MAX_KEY + 1, true, true), Err(Refusal::Invalid));
        assert_eq!(k.active(), Some(owner));
        captured(edge(&mut k, 1, 30, true), owner, 30, true);
    }
    #[test]
    fn interleaved_source_loss_and_capture_replacement_never_emit_an_unpaired_ei_edge() {
        let mut k = Keyboard::default();
        let mut seed = 0x8734_1928_u64;
        let mut observed = BTreeSet::new();
        let mut lease = 0;
        let observe = |event: Outcome, observed: &mut BTreeSet<u32>, active: Option<Recipient>| {
            if let Some((owner, key)) = event.captured {
                assert_eq!(Some(owner), active);
                if key.pressed {
                    assert!(observed.insert(key.code));
                } else {
                    assert!(observed.remove(&key.code));
                }
            }
        };
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let source = 1 + ((seed >> 8) % 64);
            let code = 1 + ((seed >> 16) % 32) as u32;
            match seed % 12 {
                0 if k.active().is_none() => {
                    lease += 1;
                    k.activate(recipient(lease, 1)).unwrap();
                }
                1 => {
                    if let Some(retired) = k.deactivate() {
                        for key in retired.releases {
                            assert!(observed.remove(&key.code));
                        }
                        assert!(observed.is_empty());
                    }
                }
                2 => {
                    for event in k.source_closed(source) {
                        observe(event, &mut observed, k.active());
                    }
                }
                _ => {
                    let active = k.active();
                    observe(
                        edge(&mut k, source, code, seed & 0x1000 != 0),
                        &mut observed,
                        active,
                    );
                }
            }
        }
        if let Some(retired) = k.deactivate() {
            for key in retired.releases {
                assert!(observed.remove(&key.code));
            }
        }
        assert!(observed.is_empty());
    }
}
