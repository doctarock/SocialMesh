//! Cryptographic primitives for SocialMesh.
//!
//! This crate wraps mature libraries (`ed25519-dalek`, `blake3`); it does not
//! implement any primitive itself. Every interoperability-relevant choice made
//! here is specified in `SPEC.md`.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use rand::RngCore;
use std::fmt;

/// Algorithm byte for BLAKE3-256 digests. Encoded in front of every digest so
/// the hash function can be migrated later without ambiguity.
pub const HASH_ALG_BLAKE3: u8 = 0x01;
/// Length of an encoded digest: one algorithm byte plus 32 digest bytes.
pub const DIGEST_ENCODED_LEN: usize = 33;
pub const PUBLIC_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

const IDENTITY_ID_CONTEXT: &str = "socialmesh v1 identity id";
const THREAD_KEY_CONTEXT: &str = "socialmesh v1 thread anonymous key";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("digest must be {DIGEST_ENCODED_LEN} bytes")]
    DigestLength,
    #[error("unknown hash algorithm 0x{0:02x}")]
    UnknownHashAlgorithm(u8),
    #[error("invalid hex")]
    InvalidHex,
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("signature verification failed")]
    BadSignature,
}

/// A content hash with its algorithm tag.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    alg: u8,
    bytes: [u8; 32],
}

/// Streaming form of [`Digest::of`].
pub struct DigestHasher(blake3::Hasher);

impl DigestHasher {
    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.0.update(data);
        self
    }
    pub fn finalize(&self) -> Digest {
        Digest { alg: HASH_ALG_BLAKE3, bytes: *self.0.finalize().as_bytes() }
    }
}

impl Digest {
    /// Hash arbitrary bytes with the current default algorithm.
    pub fn of(data: &[u8]) -> Self {
        Digest {
            alg: HASH_ALG_BLAKE3,
            bytes: *blake3::hash(data).as_bytes(),
        }
    }

    /// Incremental hashing for data too large to hold at once.
    pub fn hasher() -> DigestHasher {
        DigestHasher(blake3::Hasher::new())
    }

    pub fn algorithm(&self) -> u8 {
        self.alg
    }

    pub fn to_bytes(&self) -> [u8; DIGEST_ENCODED_LEN] {
        let mut out = [0u8; DIGEST_ENCODED_LEN];
        out[0] = self.alg;
        out[1..].copy_from_slice(&self.bytes);
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, CryptoError> {
        if b.len() != DIGEST_ENCODED_LEN {
            return Err(CryptoError::DigestLength);
        }
        if b[0] != HASH_ALG_BLAKE3 {
            return Err(CryptoError::UnknownHashAlgorithm(b[0]));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&b[1..]);
        Ok(Digest { alg: b[0], bytes })
    }

    /// Full lowercase hex form, including the algorithm byte (66 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.to_bytes())
    }

    pub fn from_hex(s: &str) -> Result<Self, CryptoError> {
        let raw = hex::decode(s.trim()).map_err(|_| CryptoError::InvalidHex)?;
        Self::from_bytes(&raw)
    }

    /// Short human-facing form: first 4 digest bytes, uppercase (e.g. `A91F2C3D`).
    /// Never use this for lookups that must be unambiguous.
    pub fn short(&self) -> String {
        hex::encode_upper(&self.bytes[..4])
    }

    /// Re-hash `data` with this digest's algorithm and compare.
    pub fn matches(&self, data: &[u8]) -> bool {
        match self.alg {
            HASH_ALG_BLAKE3 => blake3::hash(data).as_bytes() == &self.bytes,
            _ => false,
        }
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}

/// Build the exact byte string that is signed: `context || 0x00 || payload`.
/// Domain separation prevents a signature in one context being replayed in another.
pub fn signing_input(context: &str, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(context.len() + 1 + payload.len());
    v.extend_from_slice(context.as_bytes());
    v.push(0);
    v.extend_from_slice(payload);
    v
}

/// An Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    pub fn from_bytes(b: &[u8; PUBLIC_KEY_LEN]) -> Result<Self, CryptoError> {
        VerifyingKey::from_bytes(b)
            .map(PublicKey)
            .map_err(|_| CryptoError::InvalidPublicKey)
    }

    pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LEN] {
        self.0.to_bytes()
    }

    /// Identity ID = BLAKE3 derive_key("socialmesh v1 identity id", public_key).
    pub fn id(&self) -> Digest {
        Digest {
            alg: HASH_ALG_BLAKE3,
            bytes: blake3::derive_key(IDENTITY_ID_CONTEXT, &self.0.to_bytes()),
        }
    }

    /// Strict verification (rejects small-order / non-canonical edge cases).
    pub fn verify(&self, message: &[u8], sig: &[u8; SIGNATURE_LEN]) -> Result<(), CryptoError> {
        let sig = Signature::from_bytes(sig);
        self.0
            .verify_strict(message, &sig)
            .map_err(|_| CryptoError::BadSignature)
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.id().short())
    }
}

/// A locally held Ed25519 signing key. Never transmitted.
pub struct Keypair(SigningKey);

impl Keypair {
    pub fn generate() -> Self {
        Keypair(SigningKey::generate(&mut OsRng))
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Keypair(SigningKey::from_bytes(seed))
    }

    /// The 32-byte secret seed. Handle with care.
    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        self.0.sign(message).to_bytes()
    }

    /// Derive a thread-scoped anonymous keypair (README §8).
    ///
    /// `seed = BLAKE3 derive_key("socialmesh v1 thread anonymous key",
    ///                            anonymous_secret || root_thread_id)`.
    /// Without `anonymous_secret`, keys for different threads are unlinkable.
    pub fn derive_thread_key(anonymous_secret: &[u8; 32], root_thread: &Digest) -> Keypair {
        let mut material = Vec::with_capacity(32 + DIGEST_ENCODED_LEN);
        material.extend_from_slice(anonymous_secret);
        material.extend_from_slice(&root_thread.to_bytes());
        Keypair::from_seed(&blake3::derive_key(THREAD_KEY_CONTEXT, &material))
    }
}

/// Fill a fresh array from the OS CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_roundtrip() {
        let d = Digest::of(b"hello");
        assert_eq!(Digest::from_hex(&d.to_hex()).unwrap(), d);
        assert!(d.matches(b"hello"));
        assert!(!d.matches(b"hellO"));
        assert_eq!(d.to_hex().len(), 66);
    }

    #[test]
    fn rejects_unknown_alg() {
        let mut b = Digest::of(b"x").to_bytes();
        b[0] = 0x7f;
        assert_eq!(Digest::from_bytes(&b), Err(CryptoError::UnknownHashAlgorithm(0x7f)));
    }

    #[test]
    fn sign_verify() {
        let k = Keypair::generate();
        let msg = signing_input("test", b"payload");
        let sig = k.sign(&msg);
        assert!(k.public().verify(&msg, &sig).is_ok());
        assert!(k.public().verify(&signing_input("other", b"payload"), &sig).is_err());
    }

    #[test]
    fn thread_keys_are_stable_and_distinct() {
        let secret = random_bytes::<32>();
        let t1 = Digest::of(b"thread one");
        let t2 = Digest::of(b"thread two");
        let a = Keypair::derive_thread_key(&secret, &t1).public();
        let b = Keypair::derive_thread_key(&secret, &t1).public();
        let c = Keypair::derive_thread_key(&secret, &t2).public();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
