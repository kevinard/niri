use niri_config::{Action, Config};
use smithay::backend::input::{InputEvent, InputTime, TouchSlot};

use crate::tests::client::ClientId;
use crate::tests::fixture::Fixture;
use crate::tests::test_input_backend::{TestInputBackend, TestTouchEvent};

struct Touch {
    f: Fixture,
    id: ClientId,
    time: u64,
}

impl Touch {
    fn new(config: &str) -> Self {
        let config = Config::parse_mem(config).unwrap();
        let mut f = Fixture::with_config(config);
        f.add_output(1, (1920, 1080));
        f.niri().seat.add_touch();

        let id = f.add_client();
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_new_buffer();
        window.set_size(900, 1000);
        window.ack_last_and_commit();
        f.roundtrip(id);

        Self { f, id, time: 0 }
    }

    fn event(&mut self, slot: u32, x: f64, y: f64) -> TestTouchEvent {
        // 1 ms between events.
        self.time += 1000;
        TestTouchEvent {
            time: InputTime::from_micros(self.time),
            slot: TouchSlot::from(Some(slot)),
            x,
            y,
        }
    }

    /// Puts fingers down at the given relative positions, in one frame.
    fn down(&mut self, fingers: &[(u32, f64, f64)]) {
        for &(slot, x, y) in fingers {
            let event = self.event(slot, x, y);
            let state = self.f.niri_state();
            state.process_input_event(InputEvent::<TestInputBackend>::TouchDown { event });
        }
        self.frame();
    }

    /// Moves fingers to the given relative positions, in one frame.
    fn motion(&mut self, fingers: &[(u32, f64, f64)]) {
        for &(slot, x, y) in fingers {
            let event = self.event(slot, x, y);
            let state = self.f.niri_state();
            state.process_input_event(InputEvent::<TestInputBackend>::TouchMotion { event });
        }
        self.frame();
    }

    /// Lifts fingers, in one frame.
    fn up(&mut self, slots: &[u32]) {
        for &slot in slots {
            let event = self.event(slot, 0., 0.);
            let state = self.f.niri_state();
            state.process_input_event(InputEvent::<TestInputBackend>::TouchUp { event });
        }
        self.frame();
    }

    fn frame(&mut self) {
        let event = self.event(0, 0., 0.);
        let state = self.f.niri_state();
        state.process_input_event(InputEvent::<TestInputBackend>::TouchFrame { event });
        state.refresh_and_flush_clients();
    }

    /// Returns the touch events that the client received since the last call.
    fn client_events(&mut self) -> Vec<String> {
        self.f.roundtrip(self.id);
        std::mem::take(&mut self.f.client(self.id).state.touch_events)
    }
}

/// Fingers on the window, which is on the left of the output.
fn fingers(n: u32, dx: f64, dy: f64) -> Vec<(u32, f64, f64)> {
    (0..n)
        .map(|i| (i, 0.1 + 0.05 * f64::from(i) + dx, 0.5 + dy))
        .collect()
}

fn slots(n: u32) -> Vec<u32> {
    (0..n).collect()
}

#[test]
fn two_finger_touches_reach_the_client() {
    let mut t = Touch::new("");

    t.down(&fingers(2, 0., 0.));
    for i in 1..=10 {
        t.motion(&fingers(2, 0.02 * f64::from(i), 0.));
    }
    t.up(&slots(2));

    let events = t.client_events();
    assert!(!events.iter().any(|e| e == "cancel"), "{events:#?}");
    assert_eq!(events.iter().filter(|e| e.starts_with("down")).count(), 2);
    assert_eq!(
        events.iter().filter(|e| e.starts_with("motion")).count(),
        20
    );
    assert_eq!(events.iter().filter(|e| e.starts_with("up")).count(), 2);
}

#[test]
fn three_finger_swipe_cancels_client_touches() {
    let mut t = Touch::new("");

    t.down(&fingers(3, 0., 0.));
    let events = t.client_events();
    assert_eq!(events.iter().filter(|e| e.starts_with("down")).count(), 3);

    // Below the recognition threshold (16 logical pixels), the client gets the motion.
    t.motion(&fingers(3, 0.005, 0.));
    let events = t.client_events();
    assert_eq!(events.iter().filter(|e| e.starts_with("motion")).count(), 3);
    assert!(!events.iter().any(|e| e == "cancel"), "{events:#?}");

    // Past the threshold, the swipe is recognized and the client touches are cancelled.
    t.motion(&fingers(3, 0.02, 0.));
    let events = t.client_events();
    assert_eq!(
        events.last().map(String::as_str),
        Some("cancel"),
        "{events:#?}"
    );
    assert!(!events.iter().any(|e| e == "frame"), "{events:#?}");

    // Nothing else reaches the client until all fingers lift, including new fingers.
    t.motion(&fingers(3, 0.1, 0.));
    t.down(&[(3, 0.3, 0.5)]);
    t.up(&[0, 1, 2]);
    t.motion(&[(3, 0.35, 0.5)]);
    t.up(&[3]);
    assert_eq!(t.client_events(), Vec::<String>::new());

    // Then touches are delivered again.
    t.f.niri_complete_animations();
    t.down(&fingers(1, 0., 0.));
    t.up(&slots(1));
    let events = t.client_events();
    assert!(!events.iter().any(|e| e == "cancel"), "{events:#?}");
    assert!(
        events.iter().any(|e| e.starts_with("down 0")),
        "{events:#?}"
    );
    assert!(events.iter().any(|e| e == "up 0"), "{events:#?}");
}

#[test]
fn four_finger_swipe_up_opens_the_overview() {
    let mut t = Touch::new("");

    t.down(&fingers(4, 0., 0.));
    for i in 1..=10 {
        t.motion(&fingers(4, 0., -0.04 * f64::from(i)));
    }
    assert!(t.f.niri().layout.is_overview_open());
    t.up(&slots(4));
    t.f.niri_complete_animations();
    assert!(t.f.niri().layout.is_overview_open());

    let events = t.client_events();
    assert_eq!(
        events.last().map(String::as_str),
        Some("cancel"),
        "{events:#?}"
    );
}

#[test]
fn touchscreen_swipe_can_be_disabled() {
    let mut t = Touch::new("gestures { touchscreen-swipe { off; }; }");

    t.down(&fingers(4, 0., 0.));
    for i in 1..=10 {
        t.motion(&fingers(4, 0., -0.04 * f64::from(i)));
    }
    t.up(&slots(4));
    t.f.niri_complete_animations();
    assert!(!t.f.niri().layout.is_overview_open());

    let events = t.client_events();
    assert!(!events.iter().any(|e| e == "cancel"), "{events:#?}");
    assert_eq!(events.iter().filter(|e| e.starts_with("up")).count(), 4);
}

#[test]
fn four_finger_swipe_down_closes_the_overview() {
    let mut t = Touch::new("");
    t.f.niri_state().do_action(Action::OpenOverview, false);
    t.f.niri_complete_animations();

    t.down(&fingers(4, 0., -0.3));
    for i in 1..=10 {
        t.motion(&fingers(4, 0., -0.3 + 0.04 * f64::from(i)));
    }
    t.up(&slots(4));
    t.f.niri_complete_animations();
    assert!(!t.f.niri().layout.is_overview_open());
}

#[test]
fn three_finger_swipe_in_the_overview_does_not_activate_the_window() {
    let mut t = Touch::new("");
    t.f.niri_state().do_action(Action::OpenOverview, false);
    t.f.niri_complete_animations();

    // The first finger starts the overview touch grab, which activates the window under it and
    // closes the overview on a tap. The swipe must take over without that.
    // Fingers on the zoomed-out window.
    t.down(&fingers(3, 0.2, 0.));
    for i in 1..=5 {
        t.motion(&fingers(3, 0.2, 0.02 * f64::from(i)));
    }
    t.up(&slots(3));
    t.f.niri_complete_animations();
    assert!(t.f.niri().layout.is_overview_open());
}

#[test]
fn tap_in_the_overview_still_activates() {
    let mut t = Touch::new("");
    t.f.niri_state().do_action(Action::OpenOverview, false);
    t.f.niri_complete_animations();

    // Control for the test above: a tap on the window closes the overview.
    let window = (0, 0.35, 0.5);
    t.down(&[window]);
    t.up(&[0]);
    t.f.niri_complete_animations();
    assert!(!t.f.niri().layout.is_overview_open());
}
