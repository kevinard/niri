use niri_config::{Action, Config};
use niri_ipc::Event;
use smithay::backend::input::{InputEvent, InputTime, KeyState};

use crate::tests::fixture::Fixture;
use crate::tests::test_input_backend::{TestInputBackend, TestKeyboardKeyEvent};

fn set_up(config: &str) -> (Fixture, async_channel::Receiver<Event>) {
    let config = Config::parse_mem(config).unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));

    let events = f.niri().ipc_server.as_ref().unwrap().test_event_stream();
    (f, events)
}

fn open_overview(f: &mut Fixture) {
    f.niri_state().do_action(Action::OpenOverview, false);
    f.niri_state().refresh_and_flush_clients();
    assert!(f.niri().layout.is_overview_open());
    assert!(f.niri().keyboard_focus.is_overview());
}

/// Presses and releases the keys, given by XKB key name (e.g. `+LFSH +AC01 -AC01 -LFSH`).
fn input(f: &mut Fixture, keys: &str) {
    for part in keys.split_ascii_whitespace() {
        let (key_state, name) = match part.as_bytes()[0] {
            b'+' => (KeyState::Pressed, &part[1..]),
            b'-' => (KeyState::Released, &part[1..]),
            _ => panic!("a key needs a + or - prefix: {part}"),
        };

        let state = f.niri_state();
        let keyboard = state.niri.seat.get_keyboard().unwrap();
        let code = keyboard.with_xkb_state(state, |xkb| {
            let xkb = xkb.xkb().lock().unwrap();
            let keymap = unsafe { xkb.keymap() };
            keymap.key_by_name(name).unwrap()
        });

        f.niri_state()
            .process_input_event(InputEvent::<TestInputBackend>::Keyboard {
                event: TestKeyboardKeyEvent {
                    time: InputTime::from_micros(0),
                    code,
                    state: key_state,
                    count: 1,
                },
            });
        f.niri_state().refresh_and_flush_clients();
    }
}

fn typed(events: &async_channel::Receiver<Event>) -> Vec<String> {
    let mut rv = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::OverviewTyped { text } = event {
            rv.push(text);
        }
    }
    rv
}

#[test]
fn typing_in_overview_emits_text() {
    let (mut f, events) = set_up("overview { emit-typed-text; }");
    open_overview(&mut f);

    // Plain, shifted and space keys produce text.
    input(
        &mut f,
        "+AC01 -AC01 +LFSH +AC02 -AC02 -LFSH +SPCE -SPCE +AE01 -AE01",
    );
    assert_eq!(typed(&events), ["a", "S", " ", "1"]);

    // Shortcut modifiers and control keys produce no text.
    input(&mut f, "+LCTL +AC01 -AC01 -LCTL +LALT +AC01 -AC01 -LALT");
    input(&mut f, "+LWIN +AC01 -AC01 -LWIN +TAB -TAB +BKSP -BKSP");
    assert_eq!(typed(&events), Vec::<String>::new());

    // Typing doesn't close the overview; the shell decides that.
    assert!(f.niri().layout.is_overview_open());

    // The hardcoded overview binds keep working and emit nothing.
    input(&mut f, "+ESC -ESC");
    assert_eq!(typed(&events), Vec::<String>::new());
    assert!(!f.niri().layout.is_overview_open());

    // Outside the overview, nothing is reported.
    input(&mut f, "+AC01 -AC01");
    assert_eq!(typed(&events), Vec::<String>::new());
}

#[test]
fn typing_in_overview_is_off_by_default() {
    let (mut f, events) = set_up("");
    open_overview(&mut f);

    input(&mut f, "+AC01 -AC01");
    assert_eq!(typed(&events), Vec::<String>::new());
    assert!(f.niri().layout.is_overview_open());
}

#[test]
fn bound_keys_in_overview_emit_no_text() {
    let (mut f, events) = set_up(
        "
        overview { emit-typed-text; }
        binds {
            A { toggle-overview; }
        }
        ",
    );
    open_overview(&mut f);

    input(&mut f, "+AC02 -AC02 +AC01 -AC01");
    assert_eq!(typed(&events), ["s"]);
    assert!(!f.niri().layout.is_overview_open());
}
