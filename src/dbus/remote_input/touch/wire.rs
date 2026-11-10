//! Touch v1: 40-byte little-endian sequence/kind/contact/reserved (u32),
//! stream (u64), x/y (f64). 0=frame (all payload zero), 1=down, 2=motion,
//! 3=up (only contact nonzero allowed). Stream IDs refer to approved captures.
//! Replies: 16 bytes, sequence followed by three zero u32 words. Invalid input
//! closes the lease. Reads and dispatch are bounded; no read-ahead or output queue.
use std::io::{self, Read};

pub const COMMANDS_PER_DISPATCH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Frame,
    Down {
        contact: u32,
        stream: u64,
        x: f64,
        y: f64,
    },
    Motion {
        contact: u32,
        stream: u64,
        x: f64,
        y: f64,
    },
    Up {
        contact: u32,
    },
}

pub struct Reader {
    bytes: [u8; 40],
    filled: usize,
    sequence: u32,
}
impl Default for Reader {
    fn default() -> Self {
        Self {
            bytes: [0; 40],
            filled: 0,
            sequence: 0,
        }
    }
}
impl Reader {
    pub fn next(&mut self, read: &mut impl Read) -> io::Result<Option<(u32, Command)>> {
        while self.filled < self.bytes.len() {
            match read.read(&mut self.bytes[self.filled..]) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.filled += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.filled = 0;
        let word = |offset| u32::from_le_bytes(self.bytes[offset..offset + 4].try_into().unwrap());
        let wide = |offset| u64::from_le_bytes(self.bytes[offset..offset + 8].try_into().unwrap());
        let (sequence, kind, contact, reserved) = (word(0), word(4), word(8), word(12));
        let (stream, a, b) = (wide(16), wide(24), wide(32));
        if self.sequence.checked_add(1) != Some(sequence) || reserved != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let (x, y) = (f64::from_bits(a), f64::from_bits(b));
        let command = match (kind, contact, stream, a, b) {
            (0, 0, 0, 0, 0) => Command::Frame,
            (3, contact, 0, 0, 0) => Command::Up { contact },
            (kind @ 1..=2, contact, stream, _, _)
                if stream != 0
                    && x.is_finite()
                    && y.is_finite()
                    && (0.0..=1_000_000.0).contains(&x)
                    && (0.0..=1_000_000.0).contains(&y) =>
            {
                if kind == 1 {
                    Command::Down {
                        contact,
                        stream,
                        x,
                        y,
                    }
                } else {
                    Command::Motion {
                        contact,
                        stream,
                        x,
                        y,
                    }
                }
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        self.sequence = sequence;
        Ok(Some((sequence, command)))
    }
}

pub fn feedback(sequence: u32) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&sequence.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    fn packet(sequence: u32, kind: u32, contact: u32, stream: u64, x: f64, y: f64) -> Vec<u8> {
        [
            sequence.to_le_bytes().as_slice(),
            kind.to_le_bytes().as_slice(),
            contact.to_le_bytes().as_slice(),
            0u32.to_le_bytes().as_slice(),
            stream.to_le_bytes().as_slice(),
            x.to_le_bytes().as_slice(),
            y.to_le_bytes().as_slice(),
        ]
        .concat()
    }
    struct Partial(VecDeque<Option<Vec<u8>>>);
    impl Read for Partial {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(None) => Err(io::ErrorKind::WouldBlock.into()),
                Some(Some(mut data)) => {
                    let n = bytes.len().min(data.len());
                    bytes[..n].copy_from_slice(&data[..n]);
                    data.drain(..n);
                    if !data.is_empty() {
                        self.0.push_front(Some(data));
                    }
                    Ok(n)
                }
            }
        }
    }
    #[test]
    fn fragmented_frames_preserve_ids_and_do_not_consume_the_next_command() {
        let p = packet(1, 1, u32::MAX, u64::MAX, 1.5, 2.5);
        let mut input = Partial(
            [
                Some(p[..3].to_vec()),
                None,
                Some([p[3..].to_vec(), packet(2, 0, 0, 0, 0., 0.)].concat()),
            ]
            .into(),
        );
        let mut r = Reader::default();
        assert_eq!(r.next(&mut input).unwrap(), None);
        assert_eq!(
            r.next(&mut input).unwrap(),
            Some((
                1,
                Command::Down {
                    contact: u32::MAX,
                    stream: u64::MAX,
                    x: 1.5,
                    y: 2.5
                }
            ))
        );
        assert_eq!(r.next(&mut input).unwrap(), Some((2, Command::Frame)));
        assert_eq!(
            r.next(&mut input).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    #[test]
    fn rejects_invalid_coordinates_reserved_words_and_non_touch_payloads() {
        let mut reserved = packet(1, 1, 7, 1, 0., 0.);
        reserved[12] = 1;
        for p in [
            reserved,
            packet(1, 1, 0, 0, 0., 0.),
            packet(1, 2, 0, 1, -1., 0.),
            packet(1, 1, 0, 1, f64::NAN, 0.),
            packet(1, 2, 0, 1, 0., f64::INFINITY),
            packet(1, 1, 0, 1, 1_000_001., 0.),
            packet(1, 0, 1, 0, 0., 0.),
            packet(1, 3, 0, 1, 0., 0.),
            packet(1, 3, 0, 0, 1., 0.),
            packet(1, 9, 0, 0, 0., 0.),
            vec![0; 39],
        ] {
            assert!(Reader::default().next(&mut p.as_slice()).is_err());
        }
    }
    #[test]
    fn sequences_never_repeat_skip_or_wrap_and_zero_contact_is_valid() {
        for sequence in [0, 2] {
            assert!(Reader::default()
                .next(&mut packet(sequence, 0, 0, 0, 0., 0.).as_slice())
                .is_err());
        }
        let mut r = Reader::default();
        let p = packet(1, 3, 0, 0, 0., 0.);
        assert_eq!(
            r.next(&mut p.as_slice()).unwrap(),
            Some((1, Command::Up { contact: 0 }))
        );
        assert!(r.next(&mut p.as_slice()).is_err());
        let mut r = Reader {
            sequence: u32::MAX,
            ..Default::default()
        };
        assert!(r.next(&mut packet(0, 0, 0, 0, 0., 0.).as_slice()).is_err());
        assert_eq!(
            feedback(42),
            [42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }
}
