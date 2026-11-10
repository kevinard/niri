//! Finite nonblocking FD protocol for one compositor-owned shortcut lease.
//! Partial writes are terminal: the peer must observe EOF rather than an
//! ambiguous truncated event followed by another frame. The owner retires live
//! registrations whenever a socket operation fails.
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE: usize = 32 * 1024;
pub const COMMANDS_PER_DISPATCH: usize = 8;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub sequence: u64,
    pub command: Command,
}
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    List {},
    Replace {
        generation: u64,
        bindings: Vec<(u32, String)>,
    },
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Reply {
        sequence: u64,
        generation: u64,
        error: Option<String>,
        bindings: Vec<(u32, String)>,
        failed: Vec<u32>,
    },
    Changed {
        generation: u64,
        bindings: Vec<(u32, String)>,
        failed: Vec<u32>,
    },
    Activated {
        generation: u64,
        binding: u32,
        timestamp_ms: u64,
    },
    Deactivated {
        generation: u64,
        binding: u32,
        timestamp_ms: u64,
    },
}
pub struct Socket {
    stream: UnixStream,
    header: [u8; 4],
    header_bytes: usize,
    payload: Vec<u8>,
    payload_bytes: usize,
    sequence: u64,
    retired: bool,
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.retire();
    }
}
impl Socket {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            header: [0; 4],
            header_bytes: 0,
            payload: Vec::new(),
            payload_bytes: 0,
            sequence: 0,
            retired: false,
        })
    }
    /// Register readability on this clone; read and write through this Socket.
    /// The clone's open file description shares nonblocking mode and EOF state.
    pub fn registration_stream(&self) -> io::Result<UnixStream> {
        self.stream.try_clone()
    }
    pub fn retire(&mut self) {
        self.retired = true;
        let _ = self.stream.shutdown(Shutdown::Both);
    }
    pub fn next(&mut self) -> io::Result<Option<Request>> {
        if self.retired {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let result = self.read_request();
        if result.is_err() {
            self.retire();
        }
        result
    }
    fn read_request(&mut self) -> io::Result<Option<Request>> {
        loop {
            let target = if self.header_bytes < 4 {
                &mut self.header[self.header_bytes..]
            } else {
                &mut self.payload[self.payload_bytes..]
            };
            match self.stream.read(target) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) if self.header_bytes < 4 => {
                    self.header_bytes += n;
                    if self.header_bytes == 4 {
                        let length = u32::from_le_bytes(self.header) as usize;
                        if length == 0 || length > MAX_MESSAGE {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                        self.payload.resize(length, 0);
                    }
                }
                Ok(n) => {
                    self.payload_bytes += n;
                    if self.payload_bytes == self.payload.len() {
                        let request: Request = serde_json::from_slice(&self.payload)
                            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                        if request.sequence == 0 || request.sequence <= self.sequence {
                            return Err(io::ErrorKind::InvalidData.into());
                        }
                        self.sequence = request.sequence;
                        self.header_bytes = 0;
                        self.payload_bytes = 0;
                        self.payload.clear();
                        return Ok(Some(request));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
    pub fn send(&mut self, event: &Event) -> io::Result<()> {
        if self.retired {
            return Err(io::ErrorKind::NotConnected.into());
        }
        let result = (|| {
            let payload = serde_json::to_vec(event)?;
            if payload.is_empty() || payload.len() > MAX_MESSAGE {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let mut frame = Vec::with_capacity(payload.len() + 4);
            frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            frame.extend_from_slice(&payload);
            self.stream.write_all(&frame)
        })();
        if result.is_err() {
            self.retire();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pair() -> (Socket, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        b.set_write_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        (Socket::new(a).unwrap(), b)
    }
    fn assert_retired(socket: &mut Socket) {
        assert!(matches!(socket.next(), Err(e) if e.kind() == io::ErrorKind::NotConnected));
        assert!(matches!(socket.send(&event()), Err(e) if e.kind() == io::ErrorKind::NotConnected));
    }
    fn frame(value: &str) -> Vec<u8> {
        let mut bytes = (value.len() as u32).to_le_bytes().to_vec();
        bytes.extend_from_slice(value.as_bytes());
        bytes
    }
    fn list(sequence: u64) -> Vec<u8> {
        frame(&format!(
            r#"{{"sequence":{sequence},"command":{{"op":"list"}}}}"#
        ))
    }
    fn event() -> Event {
        Event::Activated {
            generation: 2,
            binding: 7,
            timestamp_ms: 5_000_000_123,
        }
    }
    #[test]
    fn fragmented_header_and_payload_never_block_or_lose_a_request() {
        let (mut socket, mut client) = pair();
        let bytes = frame(
            r#"{"sequence":1,"command":{"op":"replace","generation":7,"bindings":[[2,"CTRL+a"]]}}"#,
        );
        assert!(socket.next().unwrap().is_none());
        for (index, byte) in bytes.iter().enumerate() {
            client.write_all(&[*byte]).unwrap();
            let result = socket.next().unwrap();
            if index + 1 == bytes.len() {
                assert_eq!(
                    result.unwrap().command,
                    Command::Replace {
                        generation: 7,
                        bindings: vec![(2, "CTRL+a".into())]
                    }
                );
            } else {
                assert!(result.is_none());
            }
        }
    }
    #[test]
    fn dispatch_budget_leaves_unread_commands_ready_for_the_next_dispatch() {
        let (mut socket, mut client) = pair();
        let mut readiness = socket.registration_stream().unwrap();
        for sequence in 1..=COMMANDS_PER_DISPATCH as u64 + 1 {
            client.write_all(&list(sequence)).unwrap();
        }
        for expected in 1..=COMMANDS_PER_DISPATCH as u64 {
            assert_eq!(socket.next().unwrap().unwrap().sequence, expected);
        }
        let mut byte = [0];
        // The next nonblocking read must still yield the command left over
        // from the first dispatch, without consuming it through the clone.
        assert_eq!(
            socket.next().unwrap().unwrap().sequence,
            COMMANDS_PER_DISPATCH as u64 + 1
        );
        assert!(
            matches!(readiness.read(&mut byte), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
    }
    #[test]
    fn zero_duplicate_and_decreasing_sequences_revoke_the_stream() {
        for sequences in [[0, 1], [2, 2], [2, 1]] {
            let (mut socket, mut client) = pair();
            client.write_all(&list(sequences[0])).unwrap();
            if sequences[0] != 0 {
                assert!(socket.next().unwrap().is_some());
                client.write_all(&list(sequences[1])).unwrap();
            }
            assert!(socket.next().is_err());
            assert_retired(&mut socket);
            let mut byte = [0];
            assert_eq!(client.read(&mut byte).unwrap(), 0);
        }
    }
    #[test]
    fn oversize_empty_unknown_and_incomplete_requests_are_terminal() {
        for bytes in [
            (MAX_MESSAGE as u32 + 1).to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
            frame(r#"{"sequence":1,"command":{"op":"list","extra":true}}"#),
            frame(r#"{"sequence":1,"command":{"op":"watch_keyboard"}}"#),
            vec![7, 0],
        ] {
            let (mut socket, mut client) = pair();
            client.write_all(&bytes).unwrap();
            client.shutdown(Shutdown::Write).unwrap();
            assert!(
                socket.next().is_err(),
                "accepted malformed frame {:?}",
                String::from_utf8_lossy(&bytes)
            );
            assert_retired(&mut socket);
        }
    }
    #[test]
    fn source_timestamp_and_generation_survive_actual_socket_delivery() {
        let (mut socket, mut client) = pair();
        socket.send(&event()).unwrap();
        let mut header = [0; 4];
        client.read_exact(&mut header).unwrap();
        let mut payload = vec![0; u32::from_le_bytes(header) as usize];
        client.read_exact(&mut payload).unwrap();
        assert_eq!(serde_json::from_slice::<Event>(&payload).unwrap(), event());
    }
    #[test]
    fn blocked_output_retires_instead_of_waiting_for_the_reader() {
        let (mut socket, _client) = pair();
        let mut blocked = false;
        for _ in 0..100_000 {
            if socket.send(&event()).is_err() {
                blocked = true;
                break;
            }
        }
        assert!(blocked);
        assert_retired(&mut socket);
        assert!(socket.next().is_err());
        assert!(socket.send(&event()).is_err());
    }
    #[test]
    fn dropping_the_owner_shuts_down_even_a_retained_registration_descriptor() {
        let (socket, mut client) = pair();
        let _registration = socket.registration_stream().unwrap();
        drop(socket);
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).unwrap(), 0);
    }
}
