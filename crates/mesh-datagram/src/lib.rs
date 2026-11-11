//! Datagram mode: moving a payload (normally an offline bundle of posts) over
//! media that lose data or only go one way, such as sound, LoRa, a QR
//! sequence or a broadcast.
//!
//! There is no handshake and no reply. The payload is cut into small,
//! self-contained frames with a fountain code (RaptorQ, RFC 6330, via the
//! `raptorq` crate). The sender can emit as many frames as it likes. A
//! receiver rebuilds the payload from any set of frames slightly larger than
//! the payload, in any order, even if it starts listening halfway through.
//! Damaged frames fail their checksum and are dropped, which the code
//! treats as a lost frame.
//!
//! No link encryption: whatever goes out this way is readable by anyone in
//! range. That is acceptable only because what we send protects itself: posts
//! are signed or ownerless and verified on arrival, and attachments are sealed.
//! Only send objects that are already public.
//!
//! Frame (SPEC.md §8):
//!
//! ```text
//! "SD" | version u8 | symbol_size u8 (÷8) | transfer u32 | payload_len u32
//!      | packet_id [4] | symbol [symbol_size] | check [4]
//! ```
//!
//! `transfer` is the first 4 bytes of BLAKE3(payload): frames of the same
//! payload group together, and the rebuilt payload is checked against it.
//! `check` is the first 4 bytes of BLAKE3 over everything before it.

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation, SourceBlockEncoder};
use std::collections::{HashMap, VecDeque};

pub const MAGIC: &[u8; 2] = b"SD";
pub const VERSION: u8 = 1;
/// Bytes of a frame that aren't symbol data.
pub const OVERHEAD: usize = 2 + 1 + 1 + 4 + 4 + 4 + 4;
/// Largest payload one transfer may carry.
pub const MAX_PAYLOAD: usize = 1024 * 1024;
const ALIGNMENT: u8 = 8;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("payload is empty or larger than {MAX_PAYLOAD} bytes")]
    PayloadSize,
    #[error("symbol size must be a multiple of 8 between 8 and 2040")]
    SymbolSize,
}

fn transfer_id(payload: &[u8]) -> [u8; 4] {
    blake3::hash(payload).as_bytes()[..4].try_into().expect("4 bytes")
}

fn check(bytes: &[u8]) -> [u8; 4] {
    blake3::hash(bytes).as_bytes()[..4].try_into().expect("4 bytes")
}

fn oti(payload_len: u32, symbol_size: u16) -> ObjectTransmissionInformation {
    ObjectTransmissionInformation::new(payload_len as u64, symbol_size, 1, 1, ALIGNMENT)
}

/// Produces frames for one payload, as many as wanted.
pub struct Sender {
    header: [u8; 12],
    symbol_size: u16,
    source: VecDeque<EncodingPacket>,
    block: SourceBlockEncoder,
    next_repair: u32,
}

impl Sender {
    /// `symbol_size` is the data carried per frame (multiple of 8). Frames
    /// are `symbol_size + OVERHEAD` bytes.
    pub fn new(payload: &[u8], symbol_size: u16) -> Result<Sender, Error> {
        if payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return Err(Error::PayloadSize);
        }
        if !(8..=2040).contains(&symbol_size) || symbol_size % 8 != 0 {
            return Err(Error::SymbolSize);
        }
        let encoder = Encoder::new(payload, oti(payload.len() as u32, symbol_size));
        let mut blocks = encoder.get_block_encoders().to_vec();
        assert_eq!(blocks.len(), 1, "payloads within MAX_PAYLOAD fit one source block");
        let block = blocks.remove(0);
        let source: VecDeque<_> = block.source_packets().into();
        let mut header = [0u8; 12];
        header[..2].copy_from_slice(MAGIC);
        header[2] = VERSION;
        header[3] = (symbol_size / 8) as u8;
        header[4..8].copy_from_slice(&transfer_id(payload));
        header[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        Ok(Sender { header, symbol_size, source, block, next_repair: 0 })
    }

    /// Frames needed with no loss at all.
    pub fn source_frames(&self) -> usize {
        (self.payload_len() as usize).div_ceil(self.symbol_size as usize)
    }

    fn payload_len(&self) -> u32 {
        u32::from_be_bytes(self.header[8..12].try_into().expect("4 bytes"))
    }

    /// The next frame: the payload's own symbols first, then repair symbols
    /// for ever.
    pub fn next_frame(&mut self) -> Vec<u8> {
        let packet = match self.source.pop_front() {
            Some(p) => p,
            None => {
                let p = self.block.repair_packets(self.next_repair, 1).remove(0);
                self.next_repair += 1;
                p
            }
        };
        let mut f = Vec::with_capacity(OVERHEAD + self.symbol_size as usize);
        f.extend_from_slice(&self.header);
        f.extend_from_slice(&packet.serialize());
        let c = check(&f);
        f.extend_from_slice(&c);
        f
    }

    /// `n` frames.
    pub fn frames(&mut self, n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|_| self.next_frame()).collect()
    }
}

/// Collects frames from any number of interleaved transfers and returns each
/// payload once it is complete.
pub struct Receiver {
    pending: HashMap<[u8; 4], (Decoder, u32)>,
    order: VecDeque<[u8; 4]>,
    done: VecDeque<[u8; 4]>,
    max_pending: usize,
    /// Frames dropped as damaged or malformed.
    pub rejected: usize,
}

impl Default for Receiver {
    fn default() -> Self {
        Receiver { pending: HashMap::new(), order: VecDeque::new(), done: VecDeque::new(), max_pending: 16, rejected: 0 }
    }
}

impl Receiver {
    /// Feed one frame. Returns the payload when this frame completes it.
    /// Frames of a payload already returned are ignored.
    pub fn push(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let Some((transfer, len, symbol_size, packet)) = parse(frame) else {
            self.rejected += 1;
            return None;
        };
        if self.done.contains(&transfer) {
            return None;
        }
        if !self.pending.contains_key(&transfer) {
            // Bounded: a noisy or hostile medium can't make us hold many.
            if self.pending.len() >= self.max_pending {
                if let Some(old) = self.order.pop_front() {
                    self.pending.remove(&old);
                }
            }
            self.pending.insert(transfer, (Decoder::new(oti(len, symbol_size)), len));
            self.order.push_back(transfer);
        }
        let (decoder, expect_len) = self.pending.get_mut(&transfer).expect("inserted");
        if *expect_len != len {
            self.rejected += 1;
            return None;
        }
        let out = decoder.decode(EncodingPacket::deserialize(packet))?;
        self.pending.remove(&transfer);
        self.order.retain(|t| *t != transfer);
        if out.len() != len as usize || transfer_id(&out) != transfer {
            self.rejected += 1;
            return None;
        }
        self.done.push_back(transfer);
        if self.done.len() > 256 {
            self.done.pop_front();
        }
        Some(out)
    }
}

/// Validate a frame: (transfer, payload_len, symbol_size, packet bytes).
fn parse(f: &[u8]) -> Option<([u8; 4], u32, u16, &[u8])> {
    if f.len() < OVERHEAD + 8 || &f[..2] != MAGIC || f[2] != VERSION {
        return None;
    }
    let symbol_size = f[3] as u16 * 8;
    if symbol_size == 0 || f.len() != OVERHEAD + symbol_size as usize {
        return None;
    }
    let (body, c) = f.split_at(f.len() - 4);
    if check(body) != c {
        return None;
    }
    let transfer: [u8; 4] = f[4..8].try_into().ok()?;
    let len = u32::from_be_bytes(f[8..12].try_into().ok()?);
    if len == 0 || len as usize > MAX_PAYLOAD {
        return None;
    }
    Some((transfer, len, symbol_size, &body[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, Rng, SeedableRng};

    fn payload(n: usize, seed: u64) -> Vec<u8> {
        let mut r = StdRng::seed_from_u64(seed);
        (0..n).map(|_| r.gen()).collect()
    }

    #[test]
    fn roundtrip_without_loss() {
        let p = payload(1000, 1);
        let mut s = Sender::new(&p, 64).unwrap();
        assert_eq!(s.source_frames(), 16);
        let mut r = Receiver::default();
        let mut got = None;
        for f in s.frames(16) {
            assert_eq!(f.len(), 64 + OVERHEAD);
            got = got.or(r.push(&f));
        }
        assert_eq!(got.unwrap(), p);
    }

    #[test]
    fn heavy_loss_and_late_join() {
        let p = payload(3000, 2);
        let mut s = Sender::new(&p, 64).unwrap();
        let k = s.source_frames();
        // Receiver tunes in late (misses the first 30 frames) and loses 40%.
        let mut rng = StdRng::seed_from_u64(3);
        let mut r = Receiver::default();
        let mut used = 0;
        for f in s.frames(30) {
            let _ = f;
        }
        let out = loop {
            let f = s.next_frame();
            if rng.gen_bool(0.4) {
                continue;
            }
            used += 1;
            if let Some(out) = r.push(&f) {
                break out;
            }
            assert!(used < 3 * k, "never decoded");
        };
        assert_eq!(out, p);
        assert!(used <= k + 3, "needed {used} frames for {k} symbols");
    }

    #[test]
    fn damaged_frames_are_dropped_not_trusted() {
        let p = payload(500, 4);
        let mut s = Sender::new(&p, 32).unwrap();
        let mut r = Receiver::default();
        let mut out = None;
        for (i, mut f) in s.frames(40).into_iter().enumerate() {
            if i % 3 == 0 {
                let j = i % f.len();
                f[j] ^= 0x10; // noise flips a bit
            }
            if let Some(o) = r.push(&f) {
                out = Some(o);
                break;
            }
        }
        assert_eq!(out.unwrap(), p);
        assert!(r.rejected > 0);
    }

    #[test]
    fn interleaved_transfers_both_complete_once() {
        let (p1, p2) = (payload(700, 5), payload(900, 6));
        let (mut s1, mut s2) = (Sender::new(&p1, 48).unwrap(), Sender::new(&p2, 48).unwrap());
        let mut r = Receiver::default();
        let mut got = Vec::new();
        for _ in 0..40 {
            for s in [&mut s1, &mut s2] {
                if let Some(o) = r.push(&s.next_frame()) {
                    got.push(o);
                }
            }
        }
        got.sort_by_key(|g| g.len());
        assert_eq!(got, vec![p1, p2], "each payload exactly once");
    }

    #[test]
    fn limits() {
        assert_eq!(Sender::new(&[], 64).err(), Some(Error::PayloadSize));
        assert_eq!(Sender::new(b"x", 60).err(), Some(Error::SymbolSize));
        let mut r = Receiver::default();
        assert!(r.push(b"SD\x01").is_none());
        assert!(r.push(&[0u8; 100]).is_none());
        assert_eq!(r.rejected, 2);
    }
}
