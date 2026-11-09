//! Encrypted channel underneath the peer protocol (SPEC.md §5.0).
//!
//! Every connection starts with a Noise `NN` handshake: two fresh X25519
//! ephemeral keys, so each connection gets its own keys and recording traffic
//! now doesn't help decrypt it later (forward secrecy). Noise `NN` alone
//! authenticates nobody. Identity is proven afterwards, *inside* the encrypted
//! channel, by HELLO signatures over this channel's handshake hash
//! ([`Secured::binding`]). That binding stops a man-in-the-middle from relaying
//! one side's proof onto a different channel.
//!
//! After the handshake, data travels in records:
//!
//! ```text
//! record    = u16 ciphertext_len || ciphertext
//! plaintext = u16 data_len || data || zero padding to a multiple of 256 bytes
//! ```
//!
//! Each direction uses an implicit nonce counter starting at 0, so a dropped,
//! replayed or reordered record fails authentication and kills the connection.
//!
//! [`secure`] hands back an ordinary byte stream, so the framed peer protocol
//! runs over it unchanged.

use std::sync::Arc;

use snow::{Builder, HandshakeState, StatelessTransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

use crate::ProtocolError;

/// Noise protocol name. Changing any part of it is a protocol break.
pub const NOISE_PARAMS: &str = "Noise_NN_25519_ChaChaPoly_BLAKE2s";
/// Mixed into the handshake hash. Nodes that disagree on it fail the handshake.
pub const NOISE_PROLOGUE: &[u8] = b"socialmesh v1";

const MAX_NOISE_MESSAGE: usize = 65_535;
const TAG_LEN: usize = 16;
const MAX_PLAINTEXT: usize = MAX_NOISE_MESSAGE - TAG_LEN;
/// Largest payload per record (plaintext minus the `u16` length).
pub const MAX_RECORD_DATA: usize = MAX_PLAINTEXT - 2;
/// Record plaintexts are padded up to a multiple of this, so an observer sees
/// coarse size classes, not exact message sizes.
pub const PAD_TO: usize = 256;
/// Buffer between the plaintext side and the encryption pumps.
const PIPE_BUFFER: usize = 256 * 1024;

/// Which end of the connection we are. The dialer is the initiator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Initiator,
    Responder,
}

impl Role {
    /// Byte used in HELLO signatures, so neither side's proof can be
    /// reflected back as the other's.
    pub fn byte(self) -> u8 {
        match self {
            Role::Initiator => 0x01,
            Role::Responder => 0x02,
        }
    }

    pub fn peer(self) -> Role {
        match self {
            Role::Initiator => Role::Responder,
            Role::Responder => Role::Initiator,
        }
    }
}

/// An established encrypted channel.
pub struct Secured {
    /// Plaintext side. Read and write as a normal stream; it is encrypted on the way out.
    pub stream: DuplexStream,
    /// Noise handshake hash. Unique to this connection; both ends hold the same value.
    pub binding: [u8; 32],
}

fn noise_err(e: snow::Error) -> ProtocolError {
    ProtocolError::Secure(e.to_string())
}

/// Run the Noise handshake over `io`, then start the background record pumps.
///
/// The caller should bound this with a timeout. The pumps stop when either
/// side closes or any record fails to authenticate.
pub async fn secure<S>(io: S, role: Role) -> Result<Secured, ProtocolError>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(io);
    let params = NOISE_PARAMS.parse().map_err(noise_err)?;
    let builder = Builder::new(params).prologue(NOISE_PROLOGUE);
    let mut hs = match role {
        Role::Initiator => builder.build_initiator(),
        Role::Responder => builder.build_responder(),
    }
    .map_err(noise_err)?;

    // NN: -> e ; <- e, ee
    match role {
        Role::Initiator => {
            send_handshake(&mut hs, &mut wr).await?;
            recv_handshake(&mut hs, &mut rd).await?;
        }
        Role::Responder => {
            recv_handshake(&mut hs, &mut rd).await?;
            send_handshake(&mut hs, &mut wr).await?;
        }
    }
    if !hs.is_handshake_finished() {
        return Err(ProtocolError::Secure("handshake incomplete".into()));
    }
    let binding: [u8; 32] = hs
        .get_handshake_hash()
        .try_into()
        .map_err(|_| ProtocolError::Secure("unexpected handshake hash length".into()))?;
    let transport = Arc::new(hs.into_stateless_transport_mode().map_err(noise_err)?);

    let (plain, inner) = tokio::io::duplex(PIPE_BUFFER);
    let (inner_rd, inner_wr) = tokio::io::split(inner);
    let t2 = transport.clone();
    tokio::spawn(async move {
        // Whichever direction ends first tears down the whole connection.
        tokio::select! {
            _ = encrypt_pump(transport, inner_rd, wr) => {}
            _ = decrypt_pump(t2, rd, inner_wr) => {}
        }
    });
    Ok(Secured { stream: plain, binding })
}

async fn send_handshake<W: AsyncWrite + Unpin>(hs: &mut HandshakeState, w: &mut W) -> Result<(), ProtocolError> {
    let mut buf = vec![0u8; MAX_NOISE_MESSAGE];
    let n = hs.write_message(&[], &mut buf).map_err(noise_err)?;
    let mut out = Vec::with_capacity(2 + n);
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.extend_from_slice(&buf[..n]);
    w.write_all(&out).await?;
    w.flush().await?;
    Ok(())
}

async fn recv_handshake<R: AsyncRead + Unpin>(hs: &mut HandshakeState, r: &mut R) -> Result<(), ProtocolError> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let mut msg = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut msg).await?;
    let mut payload = vec![0u8; MAX_NOISE_MESSAGE];
    let n = hs.read_message(&msg, &mut payload).map_err(noise_err)?;
    if n != 0 {
        // NN handshake messages carry no payload in this protocol.
        return Err(ProtocolError::Secure("unexpected handshake payload".into()));
    }
    Ok(())
}

/// Padded plaintext length for `data_len` bytes of data.
fn padded_len(data_len: usize) -> usize {
    (2 + data_len).div_ceil(PAD_TO).saturating_mul(PAD_TO).min(MAX_PLAINTEXT)
}

async fn encrypt_pump<R, W>(t: Arc<StatelessTransportState>, mut plain: R, mut wire: W) -> Result<(), ProtocolError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut data = vec![0u8; MAX_RECORD_DATA];
    let mut pt = vec![0u8; MAX_PLAINTEXT];
    let mut ct = vec![0u8; 2 + MAX_NOISE_MESSAGE];
    let mut nonce = 0u64;
    loop {
        let n = plain.read(&mut data).await?;
        if n == 0 {
            let _ = wire.shutdown().await;
            return Ok(());
        }
        let len = padded_len(n);
        pt[..2].copy_from_slice(&(n as u16).to_be_bytes());
        pt[2..2 + n].copy_from_slice(&data[..n]);
        pt[2 + n..len].fill(0);
        let c = t.write_message(nonce, &pt[..len], &mut ct[2..]).map_err(noise_err)?;
        nonce += 1;
        ct[..2].copy_from_slice(&(c as u16).to_be_bytes());
        wire.write_all(&ct[..2 + c]).await?;
        wire.flush().await?;
    }
}

async fn decrypt_pump<R, W>(t: Arc<StatelessTransportState>, mut wire: R, mut plain: W) -> Result<(), ProtocolError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut ct = vec![0u8; MAX_NOISE_MESSAGE];
    let mut pt = vec![0u8; MAX_NOISE_MESSAGE];
    let mut nonce = 0u64;
    loop {
        let mut len = [0u8; 2];
        match wire.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let len = u16::from_be_bytes(len) as usize;
        if len < TAG_LEN + 2 {
            return Err(ProtocolError::Secure("record too short".into()));
        }
        wire.read_exact(&mut ct[..len]).await?;
        let n = t.read_message(nonce, &ct[..len], &mut pt).map_err(noise_err)?;
        nonce += 1;
        let data_len = u16::from_be_bytes([pt[0], pt[1]]) as usize;
        if 2 + data_len > n {
            return Err(ProtocolError::Secure("record length field exceeds record".into()));
        }
        plain.write_all(&pt[2..2 + data_len]).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pair() -> (Secured, Secured) {
        let (x, y) = tokio::io::duplex(1 << 20);
        let (a, b) = tokio::join!(secure(x, Role::Initiator), secure(y, Role::Responder));
        (a.unwrap(), b.unwrap())
    }

    #[tokio::test]
    async fn both_ends_share_binding_and_data_flows() {
        let (mut a, mut b) = pair().await;
        assert_eq!(a.binding, b.binding);
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let send = big.clone();
        let w = tokio::spawn(async move {
            a.stream.write_all(&send).await.unwrap();
            a.stream.write_all(b"tail").await.unwrap();
            a
        });
        let mut got = vec![0u8; big.len() + 4];
        b.stream.read_exact(&mut got).await.unwrap();
        assert_eq!(&got[..big.len()], &big[..]);
        assert_eq!(&got[big.len()..], b"tail");
        drop(w.await.unwrap());
        // Initiator gone: responder sees end-of-stream.
        let mut rest = Vec::new();
        b.stream.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn separate_connections_have_different_bindings() {
        let (a, _) = pair().await;
        let (c, _) = pair().await;
        assert_ne!(a.binding, c.binding);
    }

    #[tokio::test]
    async fn wire_carries_no_plaintext_and_tampering_is_fatal() {
        // Man-in-the-middle tap: forwards bytes, records them, flips one bit.
        let (x, mut tap_a) = tokio::io::duplex(1 << 20);
        let (y, mut tap_b) = tokio::io::duplex(1 << 20);
        let (a, b) = tokio::join!(
            secure(x, Role::Initiator),
            async {
                // Relay the two handshake messages untouched.
                let mut m1 = [0u8; 2 + 32];
                tap_a.read_exact(&mut m1).await.unwrap();
                tap_b.write_all(&m1).await.unwrap();
                let fut = secure(y, Role::Responder);
                let relay_back = async {
                    let mut l = [0u8; 2];
                    tap_b.read_exact(&mut l).await.unwrap();
                    let mut m2 = vec![0u8; u16::from_be_bytes(l) as usize];
                    tap_b.read_exact(&mut m2).await.unwrap();
                    tap_a.write_all(&l).await.unwrap();
                    tap_a.write_all(&m2).await.unwrap();
                };
                let (b, _) = tokio::join!(fut, relay_back);
                b
            }
        );
        let (mut a, mut b) = (a.unwrap(), b.unwrap());
        let secret = b"a political opinion that must not be read in transit";
        a.stream.write_all(secret).await.unwrap();

        let mut l = [0u8; 2];
        tap_a.read_exact(&mut l).await.unwrap();
        let mut rec = vec![0u8; u16::from_be_bytes(l) as usize];
        tap_a.read_exact(&mut rec).await.unwrap();
        assert_eq!(rec.len() % PAD_TO, TAG_LEN, "record is padded to a size class");
        assert!(!rec.windows(9).any(|w| w == b"political"), "plaintext visible on the wire");

        rec[5] ^= 0x01;
        tap_b.write_all(&l).await.unwrap();
        tap_b.write_all(&rec).await.unwrap();
        let mut buf = Vec::new();
        // Authentication fails, the pump stops, the plaintext side sees EOF with nothing delivered.
        b.stream.read_to_end(&mut buf).await.unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn padding_classes() {
        assert_eq!(padded_len(0), 256);
        assert_eq!(padded_len(254), 256);
        assert_eq!(padded_len(255), 512);
        assert_eq!(padded_len(MAX_RECORD_DATA), MAX_PLAINTEXT);
    }
}
