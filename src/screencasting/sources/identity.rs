//! Stable selection identity attached to the live Output, independent of its name.
use std::sync::atomic::{AtomicU64, Ordering};

use smithay::output::Output;

struct Identity(u64);
pub fn identity(output: &Output) -> Option<u64> {
    if let Some(id) = output.user_data().get::<Identity>() {
        return Some(id.0);
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .ok()?;
    output
        .user_data()
        .insert_if_missing_threadsafe(|| Identity(id));
    Some(output.user_data().get::<Identity>().unwrap().0)
}
#[cfg(test)]
mod tests {
    use smithay::output::{Mode, PhysicalProperties, Subpixel};

    use super::*;
    fn output() -> Output {
        Output::new(
            "same-name".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "private-test".into(),
                model: "private-test".into(),
                serial_number: "test".into(),
            },
        )
    }
    #[test]
    fn clones_share_one_identity_and_equal_names_do_not() {
        let a = output();
        let b = output();
        assert_eq!(identity(&a), identity(&a.clone()));
        assert_ne!(identity(&a), identity(&b));
    }
    #[test]
    fn replacement_cannot_revive_the_retired_output() {
        let a = output();
        let old = identity(&a).unwrap();
        let weak = a.downgrade();
        drop(a);
        assert!(weak.upgrade().is_none());
        let b = output();
        assert_ne!(identity(&b), Some(old));
        assert!(weak.upgrade().is_none());
        assert_ne!(weak, b.downgrade());
    }
    #[test]
    fn live_object_keeps_identity_across_mode_changes() {
        let a = output();
        let old = identity(&a);
        a.change_current_state(
            Some(Mode {
                size: (800, 600).into(),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        assert_eq!(identity(&a), old);
        a.change_current_state(
            Some(Mode {
                size: (1024, 768).into(),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        assert_eq!(identity(&a), old);
    }
    #[test]
    fn concurrent_clones_observe_the_same_attached_identity() {
        let a = output();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let a = a.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    identity(&a).unwrap()
                })
            })
            .collect();
        let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(ids.iter().all(|id| Some(*id) == identity(&a)));
    }
}
