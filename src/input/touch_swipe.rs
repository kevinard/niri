//! Recognition of multi-finger touchscreen swipes.
//!
//! Touches are delivered to clients as usual while a swipe is being recognized. Once three or
//! four fingers that touched down together move far enough, the swipe is recognized: the caller
//! cancels the client touches and drives a layout gesture, and all further touches are kept from
//! clients until every finger lifts.
//!
//! One and two-finger touches are never recognized as swipes.

use std::collections::HashMap;
use std::time::Duration;

use smithay::backend::input::TouchSlot;
use smithay::utils::{Logical, Point};

/// Distance that the fingers have to move together before the swipe is recognized.
///
/// Threshold copied from libadwaita, like the touchpad and touch overview gestures.
const RECOGNITION_THRESHOLD: f64 = 16.;

/// Time after the first finger touched down within which the other fingers must touch down.
///
/// Fingers added later, for example a third finger during a two-finger pinch, are not a swipe.
const FINGER_DOWN_WINDOW: Duration = Duration::from_millis(300);

/// The kind of a recognized touchscreen swipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchSwipeKind {
    /// Three-finger horizontal swipe: move the view horizontally.
    ViewOffset,
    /// Three-finger vertical swipe: switch workspaces.
    WorkspaceSwitch,
    /// Four-finger vertical swipe: open or close the overview.
    Overview,
}

/// What to do with a touch frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameAction {
    /// Deliver the frame to clients.
    Forward,
    /// Don't deliver the frame; a swipe is ongoing.
    Swallow,
    /// A swipe was recognized.
    ///
    /// Cancel the touches that clients have received instead of delivering the frame, begin the
    /// gesture at `centroid`, then apply `delta`, the movement up to now.
    Begin {
        kind: TouchSwipeKind,
        centroid: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        timestamp: Duration,
    },
    /// The fingers of an ongoing swipe moved by `delta`.
    Update {
        kind: TouchSwipeKind,
        delta: Point<f64, Logical>,
        timestamp: Duration,
    },
}

/// What to do with a touch up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpAction {
    /// Deliver the touch up to clients.
    Forward,
    /// Don't deliver the touch up; a swipe is ongoing.
    Swallow,
    /// The last finger of a swipe lifted. Don't deliver the touch up, and end the gesture.
    End(TouchSwipeKind),
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum Phase {
    /// No fingers are down.
    #[default]
    Idle,
    /// Fingers are down and might become a swipe.
    Tracking {
        first_down: Duration,
        /// Centroid of the fingers when their number last changed.
        origin: Point<f64, Logical>,
    },
    /// Fingers are down and cannot become a swipe until they all lift.
    Rejected,
    /// A swipe was recognized.
    Active {
        kind: TouchSwipeKind,
        /// Centroid of the fingers at the last frame or change of their number.
        last: Point<f64, Logical>,
    },
}

/// Recognizer for multi-finger touchscreen swipes.
///
/// Feed it the events of physical touchscreens only.
#[derive(Debug, Default)]
pub struct TouchSwipe {
    contacts: HashMap<TouchSlot, Point<f64, Logical>>,
    phase: Phase,
    last_timestamp: Duration,
}

impl TouchSwipe {
    /// Whether a swipe has been recognized and is ongoing.
    pub fn is_active(&self) -> bool {
        matches!(self.phase, Phase::Active { .. })
    }

    /// Whether the next frame may recognize a swipe.
    ///
    /// The caller checks whether a swipe is currently allowed only in this case.
    pub fn is_tracking(&self) -> bool {
        matches!(self.phase, Phase::Tracking { .. })
    }

    fn centroid(&self) -> Point<f64, Logical> {
        let count = self.contacts.len();
        if count == 0 {
            return Point::from((0., 0.));
        }

        let sum = self
            .contacts
            .values()
            .fold(Point::from((0., 0.)), |acc, pos| acc + *pos);
        sum.downscale(count as f64)
    }

    /// Handles a touch down; returns whether to keep it from clients.
    ///
    /// `allowed` is whether a swipe may start now, for example that no other touch grab is
    /// active.
    pub fn down(
        &mut self,
        slot: TouchSlot,
        pos: Point<f64, Logical>,
        timestamp: Duration,
        allowed: bool,
    ) -> bool {
        self.contacts.insert(slot, pos);
        self.last_timestamp = timestamp;
        let centroid = self.centroid();

        match &mut self.phase {
            Phase::Idle => {
                self.phase = if allowed && self.contacts.len() == 1 {
                    Phase::Tracking {
                        first_down: timestamp,
                        origin: centroid,
                    }
                } else {
                    Phase::Rejected
                };
                false
            }
            Phase::Tracking { first_down, origin } => {
                let late = timestamp.saturating_sub(*first_down) > FINGER_DOWN_WINDOW;
                if !allowed || late || self.contacts.len() > 4 {
                    self.phase = Phase::Rejected;
                } else {
                    // Measure the movement of the new set of fingers.
                    *origin = centroid;
                }
                false
            }
            Phase::Rejected => false,
            Phase::Active { last, .. } => {
                // Avoid a jump of the centroid.
                *last = centroid;
                true
            }
        }
    }

    /// Handles a touch motion; returns whether to keep it from clients.
    pub fn motion(
        &mut self,
        slot: TouchSlot,
        pos: Point<f64, Logical>,
        timestamp: Duration,
    ) -> bool {
        let Some(contact) = self.contacts.get_mut(&slot) else {
            return false;
        };
        *contact = pos;
        self.last_timestamp = timestamp;

        self.is_active()
    }

    /// Handles a touch up.
    pub fn up(&mut self, slot: TouchSlot) -> UpAction {
        if self.contacts.remove(&slot).is_none() {
            return UpAction::Forward;
        }

        let is_empty = self.contacts.is_empty();
        let centroid = self.centroid();

        match &mut self.phase {
            Phase::Idle => UpAction::Forward,
            Phase::Tracking { .. } | Phase::Rejected => {
                // Lifting a finger before the swipe is recognized means that this is not a swipe.
                self.phase = if is_empty {
                    Phase::Idle
                } else {
                    Phase::Rejected
                };
                UpAction::Forward
            }
            Phase::Active { kind, last } => {
                let kind = *kind;
                if is_empty {
                    self.phase = Phase::Idle;
                    UpAction::End(kind)
                } else {
                    // Avoid a jump of the centroid.
                    *last = centroid;
                    UpAction::Swallow
                }
            }
        }
    }

    /// Handles a touch frame.
    ///
    /// `allowed` is whether a swipe may start now; it is only looked at when
    /// [`Self::is_tracking()`] is `true`.
    pub fn frame(&mut self, allowed: bool) -> FrameAction {
        let centroid = self.centroid();
        let timestamp = self.last_timestamp;
        let fingers = self.contacts.len();

        match &mut self.phase {
            Phase::Idle | Phase::Rejected => FrameAction::Forward,
            Phase::Tracking { origin, .. } => {
                if !(3..=4).contains(&fingers) {
                    return FrameAction::Forward;
                }

                let delta = centroid - *origin;
                if delta.x * delta.x + delta.y * delta.y < RECOGNITION_THRESHOLD.powi(2) {
                    return FrameAction::Forward;
                }

                let horizontal = delta.x.abs() > delta.y.abs();
                let kind = match (fingers, horizontal) {
                    _ if !allowed => None,
                    (3, true) => Some(TouchSwipeKind::ViewOffset),
                    (3, false) => Some(TouchSwipeKind::WorkspaceSwitch),
                    (4, false) => Some(TouchSwipeKind::Overview),
                    // Like on a touchpad, horizontal four-finger swipes do nothing.
                    _ => None,
                };

                let Some(kind) = kind else {
                    self.phase = Phase::Rejected;
                    return FrameAction::Forward;
                };

                self.phase = Phase::Active {
                    kind,
                    last: centroid,
                };
                FrameAction::Begin {
                    kind,
                    centroid,
                    delta,
                    timestamp,
                }
            }
            Phase::Active { kind, last } => {
                let delta = centroid - *last;
                *last = centroid;

                if delta == Point::from((0., 0.)) {
                    FrameAction::Swallow
                } else {
                    FrameAction::Update {
                        kind: *kind,
                        delta,
                        timestamp,
                    }
                }
            }
        }
    }

    /// Forgets all fingers, for example after a touch cancel or touchscreen removal.
    ///
    /// Returns the kind of the swipe that was ongoing, if any, so that the caller can end it.
    pub fn reset(&mut self) -> Option<TouchSwipeKind> {
        let kind = match self.phase {
            Phase::Active { kind, .. } => Some(kind),
            _ => None,
        };
        self.contacts.clear();
        self.phase = Phase::Idle;
        kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(n: u32) -> TouchSlot {
        TouchSlot::from(Some(n))
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn pt(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    /// Puts `n` fingers down 1 ms apart, spaced 100 px horizontally.
    fn fingers_down(swipe: &mut TouchSwipe, n: u32) {
        for i in 0..n {
            let x = 100. * f64::from(i);
            assert!(!swipe.down(slot(i), pt(x, 500.), ms(u64::from(i)), true));
            assert_eq!(swipe.frame(true), FrameAction::Forward);
        }
    }

    /// Moves `n` fingers by (dx, dy); returns the frame action.
    fn move_fingers(
        swipe: &mut TouchSwipe,
        n: u32,
        dx: f64,
        dy: f64,
        allowed: bool,
    ) -> FrameAction {
        let mut swallowed = None;
        for i in 0..n {
            let pos = swipe.contacts[&slot(i)] + pt(dx, dy);
            let s = swipe.motion(slot(i), pos, ms(100));
            assert!(swallowed.is_none_or(|x| x == s));
            swallowed = Some(s);
        }
        swipe.frame(allowed)
    }

    fn assert_begin(action: FrameAction, expected: TouchSwipeKind) {
        match action {
            FrameAction::Begin { kind, .. } => assert_eq!(kind, expected),
            other => panic!("expected a {expected:?} swipe to begin, got {other:?}"),
        }
    }

    #[test]
    fn one_and_two_fingers_are_never_swipes() {
        for n in 1..=2 {
            let mut swipe = TouchSwipe::default();
            fingers_down(&mut swipe, n);
            for _ in 0..10 {
                assert_eq!(
                    move_fingers(&mut swipe, n, 50., 0., true),
                    FrameAction::Forward
                );
                assert_eq!(
                    move_fingers(&mut swipe, n, 0., 50., true),
                    FrameAction::Forward
                );
            }
            for i in 0..n {
                assert_eq!(swipe.up(slot(i)), UpAction::Forward);
            }
            assert_eq!(swipe.phase, Phase::Idle);
        }
    }

    #[test]
    fn three_finger_horizontal_swipe_moves_the_view() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 3);

        // Below the threshold, everything goes to clients.
        assert_eq!(
            move_fingers(&mut swipe, 3, 10., 2., true),
            FrameAction::Forward
        );
        assert!(!swipe.is_active());

        let action = move_fingers(&mut swipe, 3, 10., 2., true);
        assert_eq!(
            action,
            FrameAction::Begin {
                kind: TouchSwipeKind::ViewOffset,
                centroid: pt(120., 504.),
                delta: pt(20., 4.),
                timestamp: ms(100),
            }
        );
        assert!(swipe.is_active());

        // Further motion is kept from clients and drives the gesture.
        assert_eq!(
            move_fingers(&mut swipe, 3, -30., 0., true),
            FrameAction::Update {
                kind: TouchSwipeKind::ViewOffset,
                delta: pt(-30., 0.),
                timestamp: ms(100),
            }
        );
        assert_eq!(
            move_fingers(&mut swipe, 3, 0., 0., true),
            FrameAction::Swallow
        );

        assert_eq!(swipe.up(slot(0)), UpAction::Swallow);
        assert_eq!(swipe.up(slot(1)), UpAction::Swallow);
        assert_eq!(swipe.up(slot(2)), UpAction::End(TouchSwipeKind::ViewOffset));
        assert!(!swipe.is_active());

        // The next touch is ordinary.
        assert!(!swipe.down(slot(0), pt(0., 0.), ms(1000), true));
        assert_eq!(swipe.frame(true), FrameAction::Forward);
    }

    #[test]
    fn three_finger_vertical_swipe_switches_workspaces() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 3);
        assert_begin(
            move_fingers(&mut swipe, 3, 3., -20., true),
            TouchSwipeKind::WorkspaceSwitch,
        );
    }

    #[test]
    fn four_finger_vertical_swipe_toggles_the_overview() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 4);
        assert_begin(
            move_fingers(&mut swipe, 4, 0., -20., true),
            TouchSwipeKind::Overview,
        );
    }

    #[test]
    fn four_finger_horizontal_and_five_finger_swipes_are_rejected() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 4);
        assert_eq!(
            move_fingers(&mut swipe, 4, 20., 0., true),
            FrameAction::Forward
        );
        assert_eq!(
            move_fingers(&mut swipe, 4, 0., -50., true),
            FrameAction::Forward
        );

        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 5);
        assert_eq!(
            move_fingers(&mut swipe, 5, 0., -50., true),
            FrameAction::Forward
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn late_fingers_are_not_a_swipe() {
        let mut swipe = TouchSwipe::default();
        assert!(!swipe.down(slot(0), pt(0., 0.), ms(0), true));
        assert!(!swipe.down(slot(1), pt(100., 0.), ms(10), true));
        // A third finger long after the first ones, as during a pinch.
        assert!(!swipe.down(slot(2), pt(200., 0.), ms(1000), true));
        assert_eq!(
            move_fingers(&mut swipe, 3, 50., 0., true),
            FrameAction::Forward
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn lifting_a_finger_before_recognition_rejects_the_swipe() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 4);
        assert_eq!(swipe.up(slot(3)), UpAction::Forward);
        assert_eq!(
            move_fingers(&mut swipe, 3, 50., 0., true),
            FrameAction::Forward
        );
        assert!(!swipe.is_active());

        for i in 0..3 {
            assert_eq!(swipe.up(slot(i)), UpAction::Forward);
        }
        assert_eq!(swipe.phase, Phase::Idle);
    }

    #[test]
    fn disallowed_swipes_are_rejected_until_all_fingers_lift() {
        // Disallowed at recognition time, for example because a grab started.
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 3);
        assert_eq!(
            move_fingers(&mut swipe, 3, 50., 0., false),
            FrameAction::Forward
        );
        assert_eq!(
            move_fingers(&mut swipe, 3, 50., 0., true),
            FrameAction::Forward
        );

        // Disallowed at touch down, for example because a remote touch lease is active.
        let mut swipe = TouchSwipe::default();
        assert!(!swipe.down(slot(0), pt(0., 0.), ms(0), false));
        assert!(!swipe.down(slot(1), pt(100., 0.), ms(1), true));
        assert!(!swipe.down(slot(2), pt(200., 0.), ms(2), true));
        assert_eq!(
            move_fingers(&mut swipe, 3, 50., 0., true),
            FrameAction::Forward
        );
        assert!(!swipe.is_active());
    }

    #[test]
    fn active_swipe_keeps_new_fingers_from_clients() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 3);
        assert_begin(
            move_fingers(&mut swipe, 3, 30., 0., true),
            TouchSwipeKind::ViewOffset,
        );

        // A new finger doesn't move the centroid by itself.
        assert!(swipe.down(slot(3), pt(1000., 1000.), ms(200), true));
        assert_eq!(swipe.frame(true), FrameAction::Swallow);
        assert!(swipe.motion(slot(3), pt(1000., 1000.), ms(200)));

        // Neither does a lifted one.
        assert_eq!(swipe.up(slot(0)), UpAction::Swallow);
        assert_eq!(swipe.frame(true), FrameAction::Swallow);

        // Unknown slots are not ours.
        assert!(!swipe.motion(slot(9), pt(0., 0.), ms(200)));
        assert_eq!(swipe.up(slot(9)), UpAction::Forward);
    }

    #[test]
    fn reset_reports_the_ongoing_swipe() {
        let mut swipe = TouchSwipe::default();
        fingers_down(&mut swipe, 3);
        assert_eq!(swipe.reset(), None);

        fingers_down(&mut swipe, 3);
        assert_begin(
            move_fingers(&mut swipe, 3, 0., 30., true),
            TouchSwipeKind::WorkspaceSwitch,
        );
        assert_eq!(swipe.reset(), Some(TouchSwipeKind::WorkspaceSwitch));
        assert!(!swipe.is_active());

        // Stale ups after a reset go to clients, which already had their touches cancelled.
        assert_eq!(swipe.up(slot(0)), UpAction::Forward);
    }
}
