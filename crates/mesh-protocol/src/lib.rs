//! Peer wire protocol (SPEC.md §5).
//!
//! Every connection is first wrapped in an encrypted channel ([`secure`]).
//! Inside it, a stream is a sequence of frames: `u32 big-endian length || message`.
//! Each message is a one-byte type tag followed by canonically encoded fields.

use mesh_crypto::{signing_input, Digest, PublicKey, DIGEST_ENCODED_LEN, SIGNATURE_LEN};
use mesh_object::codec::{DecodeError, Reader, Writer};
use mesh_object::blob::BLOB_SIZE;
use mesh_object::MAX_OBJECT_SIZE;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod secure;
pub use secure::Role;

pub const PROTOCOL_VERSION: u16 = 1;
pub const HANDSHAKE_CONTEXT: &str = "socialmesh v1 handshake";
/// Largest frame accepted: one maximal object plus message overhead.
pub const MAX_FRAME: usize = MAX_OBJECT_SIZE + 1024;
/// Largest ID list in a single HAVE/WANT.
pub const MAX_IDS_PER_MESSAGE: usize = 1024;

const T_HELLO: u8 = 0x01;
// 0x02 (AUTH in early drafts) is retired: HELLO now carries its own proof.
const T_HAVE: u8 = 0x10;
const T_WANT: u8 = 0x11;
const T_OBJECT: u8 = 0x12;
const T_STEM: u8 = 0x13;
const T_STASH: u8 = 0x14;
const T_BLOB_WANT: u8 = 0x20;
const T_BLOB: u8 = 0x21;
const T_BLOB_MISSING: u8 = 0x22;
const T_PAD: u8 = 0x30;
/// Largest PAD payload.
pub const MAX_PAD: usize = 4096;

/// HELLO flag: this node accepts STEM messages (it relays for others).
pub const FLAG_ACCEPTS_STEM: u8 = 0x01;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds limit")]
    FrameTooLarge(usize),
    #[error("malformed message: {0}")]
    Decode(#[from] DecodeError),
    #[error("unknown message type 0x{0:02x}")]
    UnknownType(u8),
    #[error("invalid key or id: {0}")]
    Crypto(#[from] mesh_crypto::CryptoError),
    #[error("secure channel: {0}")]
    Secure(String),
}

// HELLO is larger than the rest but is sent once per connection; boxing it buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Identity proof, sent once by each side inside the encrypted channel.
    /// `signature` covers [`hello_input`], which binds it to this connection.
    Hello { protocol: u16, flags: u8, public_key: PublicKey, signature: [u8; SIGNATURE_LEN] },
    /// "I can supply these objects."
    Have(Vec<Digest>),
    /// "Please send me these objects."
    Want(Vec<Digest>),
    /// One encoded object. Not trusted until verified by the receiver.
    Object(Vec<u8>),
    /// Anonymity routing: "carry this object onward before it goes public".
    /// Pushed rather than requested, and handled identically whether the sender
    /// wrote it or is relaying it.
    Stem(Vec<u8>),
    /// Anonymity routing for attachments: "keep this sealed blob". Pushed to
    /// the stem successor ahead of the STEM that references it, so every node
    /// on the route ends up holding the file, not just the author.
    Stash(Vec<u8>),
    /// "Please send me these blobs" (sealed attachment pieces, SPEC.md §4).
    BlobWant(Vec<Digest>),
    /// One blob. The receiver names it by hashing it, so it can't be mislabelled.
    Blob(Vec<u8>),
    /// "I won't or can't send these right now"; ask someone else.
    BlobMissing(Vec<Digest>),
    /// Cover traffic: ignored by the receiver. Ends a decoy run of STASH the
    /// way a STEM ends a real one, so the two look alike on the wire.
    Pad(Vec<u8>),
}

impl Message {
    /// Large payloads, queued behind control messages so they don't delay sync.
    pub fn is_bulk(&self) -> bool {
        matches!(self, Message::Object(_) | Message::Blob(_) | Message::Stash(_) | Message::Pad(_))
    }
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Message::Hello { protocol, flags, public_key, signature } => {
                w.u8(T_HELLO).u16(*protocol).u8(*flags).raw(&public_key.to_bytes()).raw(signature);
            }
            Message::Have(ids) | Message::Want(ids) | Message::BlobWant(ids) | Message::BlobMissing(ids) => {
                w.u8(match self {
                    Message::Have(_) => T_HAVE,
                    Message::Want(_) => T_WANT,
                    Message::BlobWant(_) => T_BLOB_WANT,
                    _ => T_BLOB_MISSING,
                });
                w.u16(ids.len() as u16);
                for id in ids {
                    w.raw(&id.to_bytes());
                }
            }
            Message::Object(bytes) => {
                w.u8(T_OBJECT).bytes(bytes);
            }
            Message::Stem(bytes) => {
                w.u8(T_STEM).bytes(bytes);
            }
            Message::Blob(bytes) => {
                w.u8(T_BLOB).bytes(bytes);
            }
            Message::Stash(bytes) => {
                w.u8(T_STASH).bytes(bytes);
            }
            Message::Pad(bytes) => {
                w.u8(T_PAD).bytes(bytes);
            }
        }
        w.finish()
    }

    pub fn decode(buf: &[u8]) -> Result<Message, ProtocolError> {
        let mut r = Reader::new(buf);
        let msg = match r.u8()? {
            T_HELLO => Message::Hello {
                protocol: r.u16()?,
                flags: r.u8()?,
                public_key: PublicKey::from_bytes(&r.fixed::<32>()?)?,
                signature: r.fixed()?,
            },
            t @ (T_HAVE | T_WANT | T_BLOB_WANT | T_BLOB_MISSING) => {
                let n = r.u16()? as usize;
                if n > MAX_IDS_PER_MESSAGE {
                    return Err(DecodeError::TooLong { field: "ids", max: MAX_IDS_PER_MESSAGE }.into());
                }
                let mut ids = Vec::with_capacity(n);
                for _ in 0..n {
                    ids.push(Digest::from_bytes(r.take(DIGEST_ENCODED_LEN)?)?);
                }
                match t {
                    T_HAVE => Message::Have(ids),
                    T_WANT => Message::Want(ids),
                    T_BLOB_WANT => Message::BlobWant(ids),
                    _ => Message::BlobMissing(ids),
                }
            }
            T_OBJECT => Message::Object(r.bytes("object", MAX_OBJECT_SIZE)?.to_vec()),
            T_STEM => Message::Stem(r.bytes("object", MAX_OBJECT_SIZE)?.to_vec()),
            T_BLOB => Message::Blob(r.bytes("blob", BLOB_SIZE)?.to_vec()),
            T_STASH => Message::Stash(r.bytes("blob", BLOB_SIZE)?.to_vec()),
            T_PAD => Message::Pad(r.bytes("pad", MAX_PAD)?.to_vec()),
            t => return Err(ProtocolError::UnknownType(t)),
        };
        r.finish()?;
        Ok(msg)
    }
}

/// Bytes signed in HELLO. Proves the signer holds `signer`'s key *on this
/// connection*: `binding` is the encrypted channel's handshake hash, unique per
/// connection, so a proof can't be replayed or relayed onto another channel.
/// `role` is the signer's role, so one side's proof can't be reflected as the other's.
pub fn hello_input(binding: &[u8; 32], role: Role, protocol: u16, flags: u8, signer: &PublicKey) -> Vec<u8> {
    let mut payload = Vec::with_capacity(32 + 1 + 2 + 1 + 32);
    payload.extend_from_slice(binding);
    payload.push(role.byte());
    payload.extend_from_slice(&protocol.to_be_bytes());
    payload.push(flags);
    payload.extend_from_slice(&signer.to_bytes());
    signing_input(HANDSHAKE_CONTEXT, &payload)
}

pub async fn write_message<W: AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> Result<(), ProtocolError> {
    let body = msg.encode();
    if body.len() > MAX_FRAME {
        return Err(ProtocolError::FrameTooLarge(body.len()));
    }
    // One write per frame, so a frame usually fits in one encrypted record.
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Read one message. `Ok(None)` on clean end-of-stream between frames.
pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Message>, ProtocolError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        // Refuse before allocating: the length is attacker-controlled.
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Message::decode(&body).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_crypto::Keypair;

    #[test]
    fn roundtrip_all() {
        let k = Keypair::generate();
        let msgs = vec![
            Message::Hello { protocol: 1, flags: FLAG_ACCEPTS_STEM, public_key: k.public(), signature: [3; 64] },
            Message::Have(vec![Digest::of(b"a"), Digest::of(b"b")]),
            Message::Want(vec![]),
            Message::Object(vec![1, 2, 3]),
            Message::Stem(vec![4, 5]),
            Message::BlobWant(vec![Digest::of(b"c")]),
            Message::Blob(vec![6; 10]),
            Message::Stash(vec![7; 10]),
            Message::Pad(vec![0; 300]),
            Message::BlobMissing(vec![Digest::of(b"d")]),
        ];
        for m in msgs {
            assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }

    #[tokio::test]
    async fn rejects_huge_frame_header() {
        let mut data: &[u8] = &[0xff, 0xff, 0xff, 0xff];
        assert!(matches!(read_message(&mut data).await, Err(ProtocolError::FrameTooLarge(_))));
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(matches!(Message::decode(&[0x99]), Err(ProtocolError::UnknownType(0x99))));
    }
}
