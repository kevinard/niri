//! Source ownership of a combined pointer button cycle, before binding dispatch.
//! Device identities use their actual type and equality, never a printable name.
use std::any::{Any, TypeId};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Source(u64);

struct Registered {
    device: Box<dyn Any>,
    source: Source,
}

struct Cycle {
    owners: HashSet<Source>,
    origin: Option<Source>,
    routed: bool,
    grab: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ended {
    pub button: u32,
    pub routed: bool,
    pub grab: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dispatch {
    Ignore,
    Press,
    Release(Ended),
}

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct Retired {
    /// Cancel this source's pending release action even if it already released
    /// while another source kept the combined cycle alive.
    pub cancel_origin: Vec<u32>,
    pub ended: Vec<Ended>,
}

#[derive(Default)]
pub(crate) struct Buttons {
    devices: HashMap<(TypeId, u64), Vec<Registered>>,
    next_source: u64,
    held: HashMap<Source, Vec<u32>>,
    cycles: BTreeMap<u32, Cycle>,
}

fn key<D: Hash + 'static>(device: &D) -> (TypeId, u64) {
    let mut state = DefaultHasher::new();
    device.hash(&mut state);
    (TypeId::of::<D>(), state.finish())
}

impl Buttons {
    fn source<D: Eq + Hash + 'static>(&mut self, device: D, create: bool) -> Option<Source> {
        let key = key(&device);
        if let Some(bucket) = self.devices.get(&key) {
            if let Some(entry) = bucket.iter().find(|entry| {
                entry
                    .device
                    .downcast_ref::<D>()
                    .is_some_and(|old| old == &device)
            }) {
                return Some(entry.source);
            }
        }
        if !create {
            return None;
        }
        let id = self.next_source.checked_add(1)?;
        self.next_source = id;
        let source = Source(id);
        self.devices.entry(key).or_default().push(Registered {
            device: Box::new(device),
            source,
        });
        Some(source)
    }

    pub fn input<D: Eq + Hash + 'static>(
        &mut self,
        device: D,
        button: u32,
        pressed: bool,
    ) -> Dispatch {
        // wl_pointer reserves every 16-bit code. Keep existing valid codes, with
        // a finite per-device set; the remote lease has its own stricter budget.
        if button > u16::MAX.into() {
            return Dispatch::Ignore;
        }
        let Some(source) = self.source(device, pressed) else {
            return Dispatch::Ignore;
        };
        if pressed {
            let held = self.held.entry(source).or_default();
            if held.contains(&button) {
                return Dispatch::Ignore;
            }
            held.push(button);
            if let Some(cycle) = self.cycles.get_mut(&button) {
                cycle.owners.insert(source);
                Dispatch::Ignore
            } else {
                self.cycles.insert(
                    button,
                    Cycle {
                        owners: HashSet::from([source]),
                        origin: Some(source),
                        routed: false,
                        grab: None,
                    },
                );
                Dispatch::Press
            }
        } else {
            let Some(held) = self.held.get_mut(&source) else {
                return Dispatch::Ignore;
            };
            let Some(index) = held.iter().position(|old| *old == button) else {
                return Dispatch::Ignore;
            };
            held.remove(index);
            if held.is_empty() {
                self.held.remove(&source);
            }
            let cycle = self
                .cycles
                .get_mut(&button)
                .expect("held button has a cycle");
            cycle.owners.remove(&source);
            if !cycle.owners.is_empty() {
                Dispatch::Ignore
            } else {
                let cycle = self.cycles.remove(&button).unwrap();
                Dispatch::Release(Ended {
                    button,
                    routed: cycle.routed,
                    grab: cycle.grab,
                })
            }
        }
    }

    pub fn routed(&mut self, button: u32) {
        if let Some(cycle) = self.cycles.get_mut(&button) {
            cycle.routed = true;
        }
    }

    pub fn is_routed(&self, button: u32) -> bool {
        self.cycles.get(&button).is_some_and(|cycle| cycle.routed)
    }

    /// End all cycles without reusing any device's source identity.
    pub fn reset(&mut self) -> Retired {
        self.devices.clear();
        self.held.clear();
        let mut result = Retired::default();
        for (button, cycle) in std::mem::take(&mut self.cycles) {
            if cycle.origin.is_some() {
                result.cancel_origin.push(button);
            }
            result.ended.push(Ended {
                button,
                routed: cycle.routed,
                grab: cycle.grab,
            });
        }
        result
    }

    pub fn grabbed(&mut self, button: u32, grab: u32) {
        if let Some(cycle) = self.cycles.get_mut(&button) {
            cycle.grab = Some(grab);
        }
    }

    pub fn retire<D: Eq + Hash + 'static>(&mut self, device: &D) -> Retired {
        let key = key(device);
        let Some(bucket) = self.devices.get_mut(&key) else {
            return Retired::default();
        };
        let Some(index) = bucket.iter().position(|entry| {
            entry
                .device
                .downcast_ref::<D>()
                .is_some_and(|old| old == device)
        }) else {
            return Retired::default();
        };
        let source = bucket.swap_remove(index).source;
        if bucket.is_empty() {
            self.devices.remove(&key);
        }
        let mut result = Retired::default();
        for (button, cycle) in &mut self.cycles {
            if cycle.origin == Some(source) {
                cycle.origin = None;
                result.cancel_origin.push(*button);
            }
        }
        for button in self
            .held
            .remove(&source)
            .unwrap_or_default()
            .into_iter()
            .rev()
        {
            let cycle = self
                .cycles
                .get_mut(&button)
                .expect("held button has a cycle");
            cycle.owners.remove(&source);
            if cycle.owners.is_empty() {
                let cycle = self.cycles.remove(&button).unwrap();
                result.ended.push(Ended {
                    button,
                    routed: cycle.routed,
                    grab: cycle.grab,
                });
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const LEFT: u32 = 0x110;
    const RIGHT: u32 = 0x111;
    #[derive(Eq, PartialEq, Hash)]
    struct Device(u8);

    #[test]
    fn retirement_preserves_the_other_device_and_the_original_route() {
        let mut buttons = Buttons::default();
        assert_eq!(buttons.input(Device(1), LEFT, true), Dispatch::Press);
        buttons.routed(LEFT);
        buttons.grabbed(LEFT, 42);
        assert_eq!(buttons.input(Device(2), LEFT, true), Dispatch::Ignore);
        assert_eq!(
            buttons.retire(&Device(1)),
            Retired {
                cancel_origin: vec![LEFT],
                ended: vec![]
            }
        );
        assert_eq!(
            buttons.input(Device(2), LEFT, false),
            Dispatch::Release(Ended {
                button: LEFT,
                routed: true,
                grab: Some(42)
            })
        );
    }

    #[test]
    fn origin_retirement_after_its_release_still_cancels_the_pending_action() {
        let mut buttons = Buttons::default();
        buttons.input(Device(1), LEFT, true);
        buttons.input(Device(2), LEFT, true);
        assert_eq!(buttons.input(Device(1), LEFT, false), Dispatch::Ignore);
        assert_eq!(buttons.retire(&Device(1)).cancel_origin, vec![LEFT]);
        assert_eq!(
            buttons.input(Device(2), LEFT, false),
            Dispatch::Release(Ended {
                button: LEFT,
                routed: false,
                grab: None
            })
        );
    }

    #[test]
    fn a_later_source_does_not_cancel_the_first_sources_pending_action() {
        let mut buttons = Buttons::default();
        buttons.input(Device(1), LEFT, true);
        buttons.input(Device(2), LEFT, true);
        assert_eq!(buttons.retire(&Device(2)), Retired::default());
        assert_eq!(
            buttons.retire(&Device(1)),
            Retired {
                cancel_origin: vec![LEFT],
                ended: vec![Ended {
                    button: LEFT,
                    routed: false,
                    grab: None
                }]
            }
        );
    }

    #[test]
    fn consumed_presses_can_own_a_compositor_grab_without_reaching_the_seat() {
        let mut buttons = Buttons::default();
        buttons.input(Device(1), LEFT, true);
        buttons.grabbed(LEFT, 12);
        assert!(!buttons.is_routed(LEFT));
        assert_eq!(
            buttons.retire(&Device(1)).ended,
            vec![Ended {
                button: LEFT,
                routed: false,
                grab: Some(12)
            }]
        );
    }

    #[test]
    fn global_reset_ends_all_cycles_and_fresh_input_gets_a_new_source() {
        let mut buttons = Buttons::default();
        buttons.input(Device(1), LEFT, true);
        buttons.routed(LEFT);
        buttons.grabbed(LEFT, 17);
        buttons.input(Device(2), LEFT, true);
        buttons.input(Device(2), RIGHT, true);
        let old_source = buttons.next_source;
        let reset = buttons.reset();
        assert_eq!(reset.cancel_origin, vec![LEFT, RIGHT]);
        assert_eq!(
            reset.ended,
            vec![
                Ended {
                    button: LEFT,
                    routed: true,
                    grab: Some(17)
                },
                Ended {
                    button: RIGHT,
                    routed: false,
                    grab: None
                },
            ]
        );
        assert_eq!(buttons.input(Device(1), LEFT, false), Dispatch::Ignore);
        assert_eq!(buttons.retire(&Device(2)), Retired::default());
        assert_eq!(buttons.reset(), Retired::default());
        assert_eq!(buttons.input(Device(1), LEFT, true), Dispatch::Press);
        assert!(buttons.next_source > old_source);
        assert!(!buttons.is_routed(LEFT));
    }

    #[test]
    fn release_order_reverses_source_press_order_and_unknown_releases_do_nothing() {
        let mut buttons = Buttons::default();
        assert_eq!(buttons.input(Device(8), LEFT, false), Dispatch::Ignore);
        assert!(buttons.devices.is_empty());
        for button in [RIGHT, LEFT] {
            buttons.input(Device(1), button, true);
            buttons.routed(button);
        }
        let retired = buttons.retire(&Device(1));
        assert_eq!(
            retired
                .ended
                .iter()
                .map(|end| end.button)
                .collect::<Vec<_>>(),
            vec![LEFT, RIGHT]
        );
        assert_eq!(buttons.retire(&Device(1)), Retired::default());
    }

    #[test]
    fn device_hash_collisions_and_equal_values_from_different_types_are_separate() {
        #[derive(PartialEq, Eq)]
        struct Collision(u8);
        impl Hash for Collision {
            fn hash<H: Hasher>(&self, state: &mut H) {
                0u8.hash(state);
            }
        }
        let mut buttons = Buttons::default();
        buttons.input(Collision(1), LEFT, true);
        buttons.input(Collision(2), LEFT, true);
        buttons.input(Device(1), LEFT, true);
        assert!(buttons.retire(&Collision(1)).ended.is_empty());
        assert!(buttons.retire(&Collision(2)).ended.is_empty());
        assert_eq!(buttons.retire(&Device(1)).ended.len(), 1);
    }

    #[test]
    fn supported_code_bounds_and_reused_device_values_cannot_reuse_an_origin() {
        let mut buttons = Buttons::default();
        assert_eq!(buttons.input(Device(1), 0x10000, true), Dispatch::Ignore);
        assert!(buttons.devices.is_empty());
        buttons.input(Device(1), LEFT, true);
        buttons.input(Device(2), LEFT, true);
        buttons.input(Device(1), LEFT, false);
        buttons.retire(&Device(1));
        let old = buttons.next_source;
        buttons.input(Device(1), 0xffff, true);
        assert!(buttons.next_source > old);
        assert_eq!(buttons.retire(&Device(1)).cancel_origin, vec![0xffff]);
        assert_eq!(buttons.retire(&Device(2)).cancel_origin, Vec::<u32>::new());
    }

    #[test]
    fn four_device_orders_produce_one_combined_press_and_release() {
        fn permutations(values: &mut [u8], at: usize, output: &mut Vec<Vec<u8>>) {
            if at == values.len() {
                output.push(values.to_vec());
                return;
            }
            for index in at..values.len() {
                values.swap(at, index);
                permutations(values, at + 1, output);
                values.swap(at, index);
            }
        }
        let mut orders = Vec::new();
        permutations(&mut [0, 1, 2, 3], 0, &mut orders);
        for press in &orders {
            for release in &orders {
                let mut buttons = Buttons::default();
                let mut downs = 0;
                let mut ups = 0;
                for device in press {
                    if buttons.input(Device(*device), LEFT, true) == Dispatch::Press {
                        downs += 1;
                        buttons.routed(LEFT);
                    }
                }
                for device in release {
                    ups += buttons
                        .retire(&Device(*device))
                        .ended
                        .iter()
                        .filter(|end| end.routed)
                        .count();
                }
                assert_eq!((downs, ups), (1, 1));
            }
        }
    }
}
