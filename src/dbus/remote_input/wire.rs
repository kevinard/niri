//! Version 1 keyboard lease input: eight bytes, little-endian evdev key and 0/1 state.
//! A malformed transition retires the entire lease; input is never silently dropped.

use std::io::{self, Read};

use super::symbols::Key;

pub const MAX_HELD: usize = 64;
pub const FRAMES_PER_DISPATCH: usize = 32;

/// OpenKeyboardSymbols negotiates this format separately from the v1 socket.
/// Each command is four little-endian u32s: sequence, kind, value, state.
/// Kind 0 is evdev; kind 1 is XKB keysym. Sequences start at 1 and never wrap.
#[derive(Default)]
pub struct TypedReader {
    bytes: [u8; 16],
    filled: usize,
    sequence: u32,
}
impl TypedReader {
    pub fn next(&mut self, read: &mut impl Read) -> io::Result<Option<(u32, Key, bool)>> {
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
        let mut values = [0; 4];
        for (value, bytes) in values.iter_mut().zip(self.bytes.chunks_exact(4)) {
            *value = u32::from_le_bytes(bytes.try_into().unwrap());
        }
        let [sequence, kind, value, state] = values;
        if self.sequence.checked_add(1) != Some(sequence) || state > 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let key = match (kind, value) {
            (0, 1..=0x2ff) => Key::Code(value),
            (1, 1..=0x1fff_ffff) => Key::Symbol(value),
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        self.sequence = sequence;
        Ok(Some((sequence, key, state == 1)))
    }
}

/// Typed replies: sequence, status, and the authoritative four modifier masks.
/// Sequence 0/status 0 is unsolicited feedback. Every command has one reply,
/// including a logical refusal that leaves the existing held state unchanged.
pub fn feedback(sequence: u32, status: u32, mods: [u32; 4]) -> [u8; 24] {
    let mut bytes = [0; 24];
    for (chunk, value) in bytes
        .chunks_exact_mut(4)
        .zip([sequence, status, mods[0], mods[1], mods[2], mods[3]])
    {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

#[derive(Default)]
pub struct Reader {
    bytes: [u8; 8],
    filled: usize,
}

impl Reader {
    pub fn next(&mut self, read: &mut impl Read) -> io::Result<Option<(u32, bool)>> {
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
        let key = u32::from_le_bytes(self.bytes[..4].try_into().unwrap());
        let state = u32::from_le_bytes(self.bytes[4..].try_into().unwrap());
        if !(1..=0x2ff).contains(&key) || state > 1 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(Some((key, state == 1)))
    }
}

#[derive(Default)]
pub struct HeldKeys(Vec<u32>);

impl HeldKeys {
    pub fn transition(&mut self, key: u32, pressed: bool) -> io::Result<()> {
        let existing = self.0.iter().position(|k| *k == key);
        match (pressed, existing) {
            (true, None) if self.0.len() < MAX_HELD => self.0.push(key),
            (false, Some(i)) => {
                self.0.remove(i);
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
        Ok(())
    }

    pub fn keys(&self) -> &[u32] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct Partial(VecDeque<Option<Vec<u8>>>);
    impl Read for Partial {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.pop_front() {
                None => Ok(0),
                Some(None) => Err(io::ErrorKind::WouldBlock.into()),
                Some(Some(mut b)) => {
                    let n = b.len().min(buf.len());
                    buf[..n].copy_from_slice(&b[..n]);
                    b.drain(..n);
                    if !b.is_empty() {
                        self.0.push_front(Some(b));
                    }
                    Ok(n)
                }
            }
        }
    }
    fn frame(key: u32, state: u32) -> Vec<u8> {
        [key.to_le_bytes(), state.to_le_bytes()].concat()
    }
    #[test]
    fn partial_frames_survive_would_block_without_reading_ahead() {
        let data = frame(30, 1);
        let mut source = Partial(
            [
                Some(data[..3].to_vec()),
                None,
                Some([data[3..].to_vec(), frame(30, 0)].concat()),
            ]
            .into(),
        );
        let mut r = Reader::default();
        assert!(r.next(&mut source).unwrap().is_none());
        assert_eq!(r.next(&mut source).unwrap(), Some((30, true)));
        assert_eq!(r.next(&mut source).unwrap(), Some((30, false)));
        assert_eq!(
            r.next(&mut source).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    #[test]
    fn invalid_codes_states_and_truncated_eof_fail() {
        for data in [frame(0, 1), frame(0x300, 1), frame(30, 2), vec![30, 0, 0]] {
            assert!(Reader::default().next(&mut &data[..]).is_err());
        }
    }
    #[test]
    fn held_keys_reject_duplicates_unpaired_releases_and_overflow() {
        let mut h = HeldKeys::default();
        assert!(h.transition(30, false).is_err());
        h.transition(30, true).unwrap();
        assert!(h.transition(30, true).is_err());
        assert_eq!(h.keys(), &[30]);
        h.transition(30, false).unwrap();
        for key in 1..=MAX_HELD as u32 {
            h.transition(key, true).unwrap();
        }
        assert!(h.transition(100, true).is_err());
        assert_eq!(h.keys().len(), MAX_HELD);
    }

    fn typed(sequence: u32, kind: u32, value: u32, state: u32) -> Vec<u8> {
        [sequence, kind, value, state]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect()
    }
    #[test]
    fn typed_frames_preserve_partial_input_and_do_not_read_ahead() {
        let data = typed(1, 1, 0x41, 1);
        let mut source = Partial(
            [
                Some(data[..5].to_vec()),
                None,
                Some([data[5..].to_vec(), typed(2, 0, 42, 0)].concat()),
            ]
            .into(),
        );
        let mut reader = TypedReader::default();
        assert_eq!(reader.next(&mut source).unwrap(), None);
        assert_eq!(
            reader.next(&mut source).unwrap(),
            Some((1, Key::Symbol(0x41), true))
        );
        assert_eq!(
            reader.next(&mut source).unwrap(),
            Some((2, Key::Code(42), false))
        );
        assert_eq!(
            reader.next(&mut source).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
    #[test]
    fn typed_invalid_sequences_kinds_values_states_and_truncation_fail() {
        for data in [
            typed(0, 0, 42, 1),
            typed(2, 0, 42, 1),
            typed(1, 2, 42, 1),
            typed(1, 0, 0, 1),
            typed(1, 0, 0x300, 1),
            typed(1, 1, 0, 1),
            typed(1, 1, 0x2000_0000, 1),
            typed(1, 0, 42, 2),
            vec![1; 15],
        ] {
            assert!(TypedReader::default().next(&mut &data[..]).is_err());
        }
        let source = [typed(1, 0, 42, 1), typed(1, 0, 42, 0)].concat();
        let mut source = &mut &source[..];
        let mut reader = TypedReader::default();
        assert!(reader.next(&mut source).is_ok());
        assert!(reader.next(&mut source).is_err());
        let mut reader = TypedReader {
            sequence: u32::MAX,
            ..Default::default()
        };
        assert!(reader.next(&mut &typed(0, 0, 42, 1)[..]).is_err());
    }
    #[test]
    fn typed_feedback_carries_a_complete_status_and_authoritative_snapshot() {
        assert_eq!(
            feedback(12, 2, [3, 4, 5, 6]).to_vec(),
            [12u32, 2, 3, 4, 5, 6]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>()
        );
    }
}
