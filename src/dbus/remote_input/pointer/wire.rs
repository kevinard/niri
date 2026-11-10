//! Pointer lease v1: 32-byte little-endian commands (sequence, kind, a, b, c).
//! Sequence/kind are u32; payload words are u64. Sequences start at one and never
//! wrap. No read-ahead, variable allocation, descriptor input or queued output.
//! 0: barrier, zero payload. 1: relative f64 x/y, c=0. 2: button code/state,
//! c=0. 3: scroll f64 x/y, c flags (horizontal=1, vertical=2, finger=4).
//! 4: wheel signed i64 v120 x/y, c axis flags. Unselected axes must be zero.
//! 5: absolute f64 x/y, c=selected stream ID; only OpenMappedPointer accepts it.
//! Replies are 16 bytes: sequence, status=0 and two reserved zero u32 words.
//! Invalid input retires the source without acknowledging that command.

use std::collections::BTreeSet;
use std::io::{self, Read};

pub const COMMANDS_PER_DISPATCH: usize = 32;
pub const MAX_BUTTONS: usize = 64;
const MAX_AMOUNT: f64 = 1_000_000.;
const MAX_V120: i64 = 120 * 32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Barrier,
    Relative {
        x: f64,
        y: f64,
    },
    Absolute {
        x: f64,
        y: f64,
        stream: u64,
    },
    Button {
        code: u32,
        pressed: bool,
    },
    Scroll {
        x: Option<f64>,
        y: Option<f64>,
        finger: bool,
    },
    Wheel {
        x: Option<i32>,
        y: Option<i32>,
    },
}

#[derive(Default)]
pub struct Reader {
    bytes: [u8; 32],
    filled: usize,
    sequence: u32,
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
        let sequence = u32::from_le_bytes(self.bytes[0..4].try_into().unwrap());
        let kind = u32::from_le_bytes(self.bytes[4..8].try_into().unwrap());
        let a = u64::from_le_bytes(self.bytes[8..16].try_into().unwrap());
        let b = u64::from_le_bytes(self.bytes[16..24].try_into().unwrap());
        let c = u64::from_le_bytes(self.bytes[24..32].try_into().unwrap());
        if self.sequence.checked_add(1) != Some(sequence) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let amount = |bits| {
            let value = f64::from_bits(bits);
            (value.is_finite() && value.abs() <= MAX_AMOUNT).then_some(value)
        };
        let command = match (kind, a, b, c) {
            (0, 0, 0, 0) => Some(Command::Barrier),
            (1, a, b, 0) => amount(a)
                .zip(amount(b))
                .map(|(x, y)| Command::Relative { x, y }),
            (2, 1..=0xffff, 0..=1, 0) => Some(Command::Button {
                code: a as u32,
                pressed: b == 1,
            }),
            (3, a, b, flags @ 1..=7) if flags & 3 != 0 => {
                amount(a).zip(amount(b)).and_then(|(x, y)| {
                    ((flags & 1 != 0 || x == 0.) && (flags & 2 != 0 || y == 0.)).then_some(
                        Command::Scroll {
                            x: (flags & 1 != 0).then_some(x),
                            y: (flags & 2 != 0).then_some(y),
                            finger: flags & 4 != 0,
                        },
                    )
                })
            }
            (4, a, b, flags @ 1..=3) => {
                let (x, y) = (a as i64, b as i64);
                let valid = |value: i64, selected| {
                    if selected {
                        value != 0 && (-MAX_V120..=MAX_V120).contains(&value)
                    } else {
                        value == 0
                    }
                };
                (valid(x, flags & 1 != 0) && valid(y, flags & 2 != 0)).then_some(Command::Wheel {
                    x: (flags & 1 != 0).then_some(x as i32),
                    y: (flags & 2 != 0).then_some(y as i32),
                })
            }
            (5, a, b, stream) if stream != 0 => amount(a)
                .zip(amount(b))
                .filter(|(x, y)| *x >= 0. && *y >= 0.)
                .map(|(x, y)| Command::Absolute { x, y, stream }),
            _ => None,
        }
        .ok_or(io::ErrorKind::InvalidData)?;
        self.sequence = sequence;
        Ok(Some((sequence, command)))
    }
}

#[derive(Default)]
pub struct Buttons(BTreeSet<u32>);
impl Buttons {
    pub fn transition(&mut self, code: u32, pressed: bool) -> io::Result<()> {
        if pressed {
            if self.0.len() >= MAX_BUTTONS || !self.0.insert(code) {
                return Err(io::ErrorKind::InvalidData.into());
            }
        } else if !self.0.remove(&code) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
}

pub fn feedback(sequence: u32) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..4].copy_from_slice(&sequence.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    fn packet(sequence: u32, kind: u32, a: u64, b: u64, c: u64) -> Vec<u8> {
        [
            sequence.to_le_bytes().as_slice(),
            kind.to_le_bytes().as_slice(),
            a.to_le_bytes().as_slice(),
            b.to_le_bytes().as_slice(),
            c.to_le_bytes().as_slice(),
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
    fn partial_commands_keep_sequence_and_do_not_read_ahead() {
        let data = packet(1, 2, 272, 1, 0);
        let mut source = Partial(
            [
                Some(data[..7].to_vec()),
                None,
                Some([data[7..].to_vec(), packet(2, 0, 0, 0, 0)].concat()),
            ]
            .into(),
        );
        let mut r = Reader::default();
        assert_eq!(r.next(&mut source).unwrap(), None);
        assert_eq!(
            r.next(&mut source).unwrap(),
            Some((
                1,
                Command::Button {
                    code: 272,
                    pressed: true
                }
            ))
        );
        assert_eq!(r.next(&mut source).unwrap(), Some((2, Command::Barrier)));
        assert_eq!(
            r.next(&mut source).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    #[test]
    fn absolute_commands_keep_stream_identity_and_reject_invalid_coordinates() {
        let data = packet(1, 5, 123.5f64.to_bits(), 456.25f64.to_bits(), u64::MAX);
        assert_eq!(
            Reader::default().next(&mut data.as_slice()).unwrap(),
            Some((
                1,
                Command::Absolute {
                    x: 123.5,
                    y: 456.25,
                    stream: u64::MAX
                }
            ))
        );
        for (x, y, stream) in [
            (0., 0., 0),
            (-1., 1., 1),
            (1., -1., 1),
            (f64::NAN, 1., 1),
            (1., f64::INFINITY, 1),
            (1_000_001., 1., 1),
        ] {
            let data = packet(1, 5, x.to_bits(), y.to_bits(), stream);
            assert_eq!(
                Reader::default()
                    .next(&mut data.as_slice())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
    #[test]
    fn nonfinite_excessive_reserved_and_unselected_payloads_fail() {
        for data in [
            packet(0, 0, 0, 0, 0),
            packet(2, 0, 0, 0, 0),
            packet(1, 0, 1, 0, 0),
            packet(1, 99, 0, 0, 0),
            packet(1, 1, f64::NAN.to_bits(), 0, 0),
            packet(1, 1, f64::INFINITY.to_bits(), 0, 0),
            packet(1, 1, (-1_000_001f64).to_bits(), 0, 0),
            packet(1, 1, 0, 0, 1),
            packet(1, 2, 0, 1, 0),
            packet(1, 2, 0x10000, 1, 0),
            packet(1, 2, 272, 2, 0),
            packet(1, 2, 272, 1, 1),
            packet(1, 3, 0, 0, 0),
            packet(1, 3, 0, 0, 4),
            packet(1, 3, 0, 1f64.to_bits(), 1),
            packet(1, 3, 0, 0, 8),
            packet(1, 4, 0, 3841, 2),
            packet(1, 4, 0, 0, 2),
            packet(1, 4, u64::MAX, 120, 2),
            packet(1, 4, 0, 120, 6),
            vec![0; 31],
        ] {
            assert!(Reader::default().next(&mut &data[..]).is_err(), "{data:?}");
        }
    }
    #[test]
    fn valid_payloads_preserve_axis_selection_sign_and_stop() {
        for (kind, a, b, c, expected) in [
            (
                1,
                1.5f64.to_bits(),
                (-2f64).to_bits(),
                0,
                Command::Relative { x: 1.5, y: -2. },
            ),
            (
                2,
                0xffff,
                0,
                0,
                Command::Button {
                    code: 0xffff,
                    pressed: false,
                },
            ),
            (
                3,
                0,
                0,
                6,
                Command::Scroll {
                    x: None,
                    y: Some(0.),
                    finger: true,
                },
            ),
            (
                3,
                2f64.to_bits(),
                0,
                1,
                Command::Scroll {
                    x: Some(2.),
                    y: None,
                    finger: false,
                },
            ),
            (
                4,
                (-120i64) as u64,
                240,
                3,
                Command::Wheel {
                    x: Some(-120),
                    y: Some(240),
                },
            ),
        ] {
            assert_eq!(
                Reader::default()
                    .next(&mut &packet(1, kind, a, b, c)[..])
                    .unwrap(),
                Some((1, expected))
            );
        }
    }
    #[test]
    fn sequences_cannot_repeat_skip_or_wrap() {
        let data = [packet(1, 0, 0, 0, 0), packet(1, 0, 0, 0, 0)].concat();
        let mut source = &data[..];
        let mut r = Reader::default();
        assert!(r.next(&mut source).is_ok());
        assert!(r.next(&mut source).is_err());
        let mut r = Reader {
            sequence: u32::MAX,
            ..Default::default()
        };
        assert!(r.next(&mut &packet(0, 0, 0, 0, 0)[..]).is_err());
    }
    #[test]
    fn held_limit_duplicates_and_unpaired_release_fail_before_dispatch() {
        let mut buttons = Buttons::default();
        assert!(buttons.transition(272, false).is_err());
        buttons.transition(272, true).unwrap();
        assert!(buttons.transition(272, true).is_err());
        buttons.transition(272, false).unwrap();
        for code in 1..=MAX_BUTTONS as u32 {
            buttons.transition(code, true).unwrap();
        }
        assert!(buttons.transition(100, true).is_err());
        assert_eq!(buttons.0.len(), MAX_BUTTONS);
        buttons.transition(1, false).unwrap();
        buttons.transition(100, true).unwrap();
    }
    #[test]
    fn replies_have_only_the_acknowledged_sequence() {
        assert_eq!(
            feedback(9).to_vec(),
            [9u32, 0, 0, 0]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }
}
