//! Compositor-thread lease owner. D-Bus admission and input policy are supplied
//! by Niri; this module owns FD framing, logical registrations, generation
//! changes and directed press events together, including transport failure.
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::net::UnixStream;

use super::engine::{Definition, Description, Engine, Error};
use super::presses::{Edge, Update};
use super::registry::{Conflict, Error as RegistryError, Slot};
use super::resolver::Snapshot;
use super::wire::{Command, Event, Socket, COMMANDS_PER_DISPATCH};

#[derive(Default)]
pub struct Host {
    engine: Engine,
    peers: BTreeMap<u64, Socket>,
}
pub struct Registration {
    pub id: u64,
    pub stream: UnixStream,
}
fn bindings(description: Description) -> Vec<(u32, String)> {
    description
        .bindings
        .into_iter()
        .map(|d| (d.id, d.trigger))
        .collect()
}
fn failure(error: Error) -> (&'static str, Vec<u32>) {
    match error {
        Error::Unavailable => ("unavailable", Vec::new()),
        Error::StaleGeneration => ("stale_generation", Vec::new()),
        Error::InvalidDefinitions => ("invalid_definitions", Vec::new()),
        Error::Resolution(errors) => ("unresolved", errors.into_iter().map(|(id, _)| id).collect()),
        Error::Registration(RegistryError::Conflicts(conflicts)) => (
            "conflict",
            conflicts
                .into_iter()
                .map(|c| match c {
                    Conflict::Reserved { binding }
                    | Conflict::Occupied { binding, .. }
                    | Conflict::DuplicateTrigger { binding, .. } => binding,
                })
                .collect(),
        ),
        Error::Registration(RegistryError::Limits) => ("limits", Vec::new()),
        Error::Registration(RegistryError::Exhausted) => ("exhausted", Vec::new()),
        Error::Registration(RegistryError::InvalidBinding) => ("invalid_definitions", Vec::new()),
        Error::Registration(RegistryError::UnknownLease) => ("closed", Vec::new()),
    }
}
impl Host {
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
    pub fn close_all(&mut self, time_us: u64) {
        let ids: Vec<_> = self.peers.keys().copied().collect();
        for id in ids {
            self.close(id, time_us);
        }
    }
    pub fn reconcile(&mut self, down: &BTreeSet<u32>, time_us: u64) {
        let edges = self.engine.reconcile(down, time_us);
        self.edges(edges, time_us);
    }
    pub fn open(&mut self, stream: UnixStream) -> io::Result<Registration> {
        let socket = Socket::new(stream)?;
        let registration = socket.registration_stream()?;
        let id = self
            .engine
            .open()
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        self.peers.insert(id, socket);
        Ok(Registration {
            id,
            stream: registration,
        })
    }
    pub fn contains(&self, id: u64) -> bool {
        self.peers.contains_key(&id)
    }
    pub fn close(&mut self, id: u64, time_us: u64) {
        self.peers.remove(&id);
        // The closed FD has no recipient. Its EOF revokes the public owner;
        // captured cycles remain suppression tombstones until seat-up.
        self.engine.close(id, time_us);
    }
    fn send(&mut self, id: u64, event: &Event, time_us: u64) -> bool {
        if self
            .peers
            .get_mut(&id)
            .is_some_and(|socket| socket.send(event).is_ok())
        {
            return true;
        }
        self.close(id, time_us);
        false
    }
    fn edges(&mut self, edges: Vec<Edge>, time_us: u64) {
        for edge in edges {
            let event = if edge.activated {
                Event::Activated {
                    generation: edge.target.generation,
                    binding: edge.target.binding,
                    timestamp_ms: edge.timestamp_ms,
                }
            } else {
                Event::Deactivated {
                    generation: edge.target.generation,
                    binding: edge.target.binding,
                    timestamp_ms: edge.timestamp_ms,
                }
            };
            self.send(edge.target.lease, &event, time_us);
        }
    }
    fn update(&mut self, update: Update, time_us: u64) -> bool {
        self.edges(update.edges, time_us);
        if let Some(target) = update.overflow {
            self.close(target.lease, time_us);
        }
        update.captured
    }
    /// Read at most eight commands. True keeps the readability registration;
    /// false removes it. Both malformed input and output backpressure revoke
    /// the corresponding registration before returning to the event loop.
    pub fn dispatch(
        &mut self,
        id: u64,
        mut reserved: impl FnMut(Slot, u8) -> bool,
        time_us: u64,
    ) -> bool {
        for _ in 0..COMMANDS_PER_DISPATCH {
            let Some(socket) = self.peers.get_mut(&id) else {
                return false;
            };
            let request = match socket.next() {
                Ok(Some(request)) => request,
                Ok(None) => return true,
                Err(_) => {
                    self.close(id, time_us);
                    return false;
                }
            };
            let mut error = None;
            let mut failed = Vec::new();
            if let Command::Replace {
                generation,
                bindings,
            } = request.command
            {
                let proposed = bindings
                    .into_iter()
                    .map(|(id, trigger)| Definition { id, trigger })
                    .collect();
                match self
                    .engine
                    .replace(id, generation, proposed, &mut reserved, time_us)
                {
                    Ok(changed) => self.edges(changed.deactivated, time_us),
                    Err(reason) => {
                        let (code, ids) = failure(reason);
                        error = Some(code.to_owned());
                        failed = ids;
                    }
                }
            }
            let Some(description) = self.engine.describe(id) else {
                self.close(id, time_us);
                return false;
            };
            let reply = Event::Reply {
                sequence: request.sequence,
                generation: description.generation,
                error,
                bindings: bindings(description),
                failed,
            };
            if !self.send(id, &reply, time_us) {
                return false;
            }
        }
        self.contains(id)
    }
    pub fn revalidate(
        &mut self,
        map: Option<Snapshot>,
        reserved: impl FnMut(Slot, u8) -> bool,
        time_us: u64,
    ) {
        let changes = self.engine.revalidate(map, reserved, time_us);
        for change in changes {
            self.edges(change.deactivated, time_us);
            if change.description.generation == 0 {
                self.close(change.lease, time_us);
                continue;
            }
            let event = Event::Changed {
                generation: change.description.generation,
                bindings: bindings(change.description),
                failed: change.failed,
            };
            self.send(change.lease, &event, time_us);
        }
    }
    pub fn raw_edge(&mut self, source: u64, code: u32, pressed: bool, time_us: u64) -> bool {
        let update = self.engine.raw_edge(source, code, pressed, time_us);
        self.update(update, time_us)
    }
    /// The caller has already applied normal compositor input policy and the
    /// seat accepted this as a new combined press. This never handles repeats.
    pub fn accepted_press(
        &mut self,
        source: u64,
        code: u32,
        layout: u32,
        effective_mods: u32,
        time_us: u64,
    ) -> bool {
        match self
            .engine
            .accepted_press(source, code, layout, effective_mods, time_us)
        {
            Ok(Some(update)) => self.update(update, time_us),
            Ok(None) => false,
            Err(_) => {
                // Bounded press bookkeeping cannot retain truthful authority.
                // Retire rather than acknowledging an untracked activation.
                let ids: Vec<_> = self.peers.keys().copied().collect();
                for id in ids {
                    self.close(id, time_us);
                }
                false
            }
        }
    }
    pub fn seat_key(&mut self, code: u32, down: bool, time_us: u64) {
        let edges = self.engine.seat_key(code, down, time_us);
        self.edges(edges, time_us);
    }
    pub fn source_closed(&mut self, source: u64, down: &BTreeSet<u32>, time_us: u64) {
        let edges = self.engine.source_closed(source, time_us);
        self.edges(edges, time_us);
        let edges = self.engine.reconcile(down, time_us);
        self.edges(edges, time_us);
    }
    pub fn suspend(&mut self, time_us: u64) {
        let edges = self.engine.suspend(time_us);
        self.edges(edges, time_us);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::Shutdown;
    use std::time::Duration;

    use super::super::xkb;
    use super::*;
    fn map() -> Snapshot {
        let map = xkb::Keymap::new_from_names(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        Snapshot::new(&map).unwrap()
    }
    #[test]
    fn bulk_revocation_retains_held_suppression_until_actual_seat_reconciliation() {
        let mut host = ready();
        let (_, mut first) = bound(&mut host, "a");
        let (_, _second) = bound(&mut host, "b");
        assert!(!host.is_empty());
        assert!(host.accepted_press(11, 38, 0, 0, 2000));
        event(&mut first);
        host.close_all(3000);
        assert!(host.is_empty());
        host.reconcile(&BTreeSet::from([38]), 4000);
        let (_, mut replacement) = bound(&mut host, "a");
        assert!(host.raw_edge(12, 38, true, 5000));
        assert!(host.accepted_press(12, 38, 0, 0, 5000));
        host.reconcile(&BTreeSet::new(), 6000);
        assert!(host.accepted_press(13, 38, 0, 0, 7000));
        assert!(matches!(
            event(&mut replacement),
            Event::Activated {
                timestamp_ms: 7,
                ..
            }
        ));
    }
    fn ready() -> Host {
        let mut h = Host::default();
        h.revalidate(Some(map()), |_, _| false, 0);
        h
    }
    fn open(h: &mut Host) -> (Registration, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (h.open(a).unwrap(), b)
    }
    fn request(client: &mut UnixStream, sequence: u64, command: serde_json::Value) {
        let payload =
            serde_json::to_vec(&serde_json::json!({"sequence":sequence,"command":command}))
                .unwrap();
        client
            .write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        client.write_all(&payload).unwrap();
    }
    fn replace(client: &mut UnixStream, sequence: u64, generation: u64, trigger: &str) {
        request(
            client,
            sequence,
            serde_json::json!({"op":"replace","generation":generation,"bindings":[[7,trigger]]}),
        );
    }
    fn event(client: &mut UnixStream) -> Event {
        let mut header = [0; 4];
        client.read_exact(&mut header).unwrap();
        let mut payload = vec![0; u32::from_le_bytes(header) as usize];
        client.read_exact(&mut payload).unwrap();
        serde_json::from_slice(&payload).unwrap()
    }
    fn bound(h: &mut Host, trigger: &str) -> (Registration, UnixStream) {
        let (registration, mut client) = open(h);
        replace(&mut client, 1, 0, trigger);
        assert!(h.dispatch(registration.id, |_, _| false, 1000));
        assert!(matches!(
            event(&mut client),
            Event::Reply {
                generation: 1,
                error: None,
                ..
            }
        ));
        (registration, client)
    }
    #[test]
    fn fd_registration_and_directed_events_preserve_press_identity_and_event_time() {
        let mut h = ready();
        let (registration, mut client) = bound(&mut h, "a");
        assert!(registration.stream.take_error().unwrap().is_none());
        assert!(h.accepted_press(11, 38, 0, 0, 5_000_000_123_000));
        assert_eq!(
            event(&mut client),
            Event::Activated {
                generation: 1,
                binding: 7,
                timestamp_ms: 5_000_000_123
            }
        );
        assert!(h.raw_edge(11, 38, false, 5_000_000_124_000));
        assert_eq!(
            event(&mut client),
            Event::Deactivated {
                generation: 1,
                binding: 7,
                timestamp_ms: 5_000_000_124
            }
        );
        h.seat_key(38, false, 5_000_000_124_000);
    }
    #[test]
    fn canceled_fd_frees_its_conflicts_and_keeps_held_release_suppressed() {
        let mut h = ready();
        let (first, client1) = bound(&mut h, "a");
        let (second, mut client2) = open(&mut h);
        assert!(h.accepted_press(11, 38, 0, 0, 2000));
        replace(&mut client2, 1, 0, "a");
        h.dispatch(second.id, |_, _| false, 3000);
        assert!(
            matches!(event(&mut client2), Event::Reply { error:Some(error), generation:0, .. } if error == "conflict")
        );
        client1.shutdown(Shutdown::Both).unwrap();
        assert!(!h.dispatch(first.id, |_, _| false, 4000));
        replace(&mut client2, 2, 0, "a");
        h.dispatch(second.id, |_, _| false, 5000);
        assert!(matches!(
            event(&mut client2),
            Event::Reply {
                error: None,
                generation: 1,
                ..
            }
        ));
        assert!(h.raw_edge(11, 38, false, 6000));
        h.seat_key(38, false, 6000);
        assert!(h.accepted_press(12, 38, 0, 0, 7000));
        assert!(matches!(
            event(&mut client2),
            Event::Activated { generation: 1, .. }
        ));
    }
    #[test]
    fn revalidation_deactivates_before_changed_and_rejects_a_stale_configuration() {
        let mut h = ready();
        let (registration, mut client) = bound(&mut h, "a");
        h.accepted_press(11, 38, 0, 0, 2000);
        event(&mut client);
        h.revalidate(Some(map()), |slot, _| slot.code == 38, 3000);
        assert!(matches!(
            event(&mut client),
            Event::Deactivated { generation: 1, .. }
        ));
        assert!(
            matches!(event(&mut client), Event::Changed { generation:3, bindings, failed } if bindings.is_empty() && failed == vec![7])
        );
        replace(&mut client, 2, 1, "q");
        h.dispatch(registration.id, |_, _| false, 4000);
        assert!(
            matches!(event(&mut client), Event::Reply { generation:3, error:Some(error), bindings, .. } if error == "stale_generation" && bindings.is_empty())
        );
    }
    #[test]
    fn source_removal_and_suspension_emit_one_deactivation_without_a_release_lookup() {
        let mut h = ready();
        let (_, mut client) = bound(&mut h, "a");
        h.accepted_press(11, 38, 0, 0, 2000);
        event(&mut client);
        h.raw_edge(12, 38, true, 3000);
        h.source_closed(11, &BTreeSet::from([38]), 4000);
        h.source_closed(12, &BTreeSet::new(), 5000);
        assert!(matches!(
            event(&mut client),
            Event::Deactivated {
                timestamp_ms: 5,
                ..
            }
        ));
        h.accepted_press(13, 38, 0, 0, 6000);
        event(&mut client);
        h.suspend(7000);
        assert!(matches!(
            event(&mut client),
            Event::Deactivated {
                timestamp_ms: 7,
                ..
            }
        ));
        h.suspend(8000);
        assert!(h.raw_edge(13, 38, false, 9000));
        h.seat_key(38, false, 9000);
        h.accepted_press(13, 38, 0, 0, 10000);
        assert!(matches!(
            event(&mut client),
            Event::Activated {
                timestamp_ms: 10,
                ..
            }
        ));
    }
    #[test]
    fn malformed_control_revokes_only_its_own_lease_and_dispatch_is_bounded() {
        let mut h = ready();
        let (bad, mut client1) = bound(&mut h, "a");
        let (good, mut client2) = bound(&mut h, "b");
        request(&mut client1, 1, serde_json::json!({"op":"list"}));
        assert!(!h.dispatch(bad.id, |_, _| false, 2000));
        assert!(h.contains(good.id));
        for sequence in 2..=10 {
            request(&mut client2, sequence, serde_json::json!({"op":"list"}));
        }
        assert!(h.dispatch(good.id, |_, _| false, 3000));
        for expected in 2..=9 {
            assert!(
                matches!(event(&mut client2), Event::Reply { sequence, .. } if sequence == expected)
            );
        }
        assert!(h.dispatch(good.id, |_, _| false, 4000));
        assert!(matches!(
            event(&mut client2),
            Event::Reply { sequence: 10, .. }
        ));
    }
    #[test]
    fn unread_events_revoke_the_slow_peer_without_blocking_another_lease() {
        let mut h = ready();
        let (slow, _unread) = bound(&mut h, "a");
        let (good, mut client2) = bound(&mut h, "b");
        for time in 2..100_000 {
            h.accepted_press(11, 38, 0, 0, time * 1000);
            h.raw_edge(11, 38, false, time * 1000);
            h.seat_key(38, false, time * 1000);
            if !h.contains(slow.id) {
                break;
            }
        }
        assert!(!h.contains(slow.id));
        assert!(h.contains(good.id));
        assert!(h.accepted_press(12, 56, 0, 0, 100_000_000));
        assert!(matches!(event(&mut client2), Event::Activated { .. }));
    }
}
