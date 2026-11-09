//! Sealed attachments (README §11, SPEC.md §4).
//!
//! A file is encrypted the moment it is attached, with a fresh random key that
//! travels only inside the post. It becomes a set of **blobs**:
//!
//! ```text
//! post ──attachment {manifest id, key}──► manifest blob ──► chunk blob 0 … n-1
//! ```
//!
//! Every blob, manifest or chunk, is exactly [`BLOB_SIZE`] bytes of
//! ChaCha20-Poly1305 ciphertext, named by its BLAKE3 hash. To anyone without
//! the post, all blobs look alike: no file type, no size beyond "one more
//! piece", no way to tell a manifest from a chunk or which pieces belong
//! together. Nodes can still check integrity (hash) without being able to read.
//!
//! The key is random, not derived from the content. Content-derived keys would
//! let someone holding a known file check whether a node had cached it.

use crate::codec::{Reader, Writer};
use crate::ObjectError;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use mesh_crypto::{Digest, DIGEST_ENCODED_LEN};

/// Every blob on the wire and on disk is exactly this long.
pub const BLOB_SIZE: usize = 64 * 1024;
const TAG_LEN: usize = 16;
/// File bytes carried per chunk.
pub const CHUNK_PLAINTEXT: usize = BLOB_SIZE - TAG_LEN;
pub const MANIFEST_MAGIC: &[u8; 4] = b"SMMF";
pub const MANIFEST_VERSION: u8 = 1;
const MANIFEST_HEADER: usize = 4 + 1 + 8 + DIGEST_ENCODED_LEN + 4;
/// Chunks one manifest blob can list.
pub const MAX_CHUNKS: usize = (CHUNK_PLAINTEXT - MANIFEST_HEADER) / DIGEST_ENCODED_LEN;
/// Largest attachment (about 124 MiB).
pub const MAX_ATTACHMENT_SIZE: u64 = (MAX_CHUNKS * CHUNK_PLAINTEXT) as u64;
/// Associated data for every blob, for domain separation.
const AAD: &[u8] = b"socialmesh v1 blob";
const NONCE_MANIFEST: u8 = 0x01;
const NONCE_CHUNK: u8 = 0x02;

// A blob must fit in one wire frame, which is sized for the largest object.
const _: () = assert!(BLOB_SIZE <= crate::MAX_OBJECT_SIZE);

/// Decrypted manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub size: u64,
    /// BLAKE3 of the complete plaintext file, checked after reassembly.
    pub content: Digest,
    /// IDs of the chunk blobs, in file order.
    pub chunks: Vec<Digest>,
}

impl Manifest {
    /// File bytes in chunk `i`.
    pub fn chunk_len(&self, i: usize) -> usize {
        (self.size as usize - i * CHUNK_PLAINTEXT).min(CHUNK_PLAINTEXT)
    }
}

/// A file after sealing: what goes in the post, and the blobs to store.
pub struct Sealed {
    pub manifest_id: Digest,
    pub key: [u8; 32],
    pub size: u64,
    /// Manifest blob first, then chunk blobs in order.
    pub blobs: Vec<Vec<u8>>,
}

fn nonce(kind: u8, index: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[0] = kind;
    n[4..].copy_from_slice(&index.to_be_bytes());
    n
}

fn encrypt(key: &[u8; 32], kind: u8, index: u64, plaintext: &[u8]) -> Vec<u8> {
    debug_assert_eq!(plaintext.len(), CHUNK_PLAINTEXT);
    ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(Nonce::from_slice(&nonce(kind, index)), Payload { msg: plaintext, aad: AAD })
        .expect("encryption of bounded input cannot fail")
}

fn decrypt(key: &[u8; 32], kind: u8, index: u64, blob: &[u8]) -> Result<Vec<u8>, ObjectError> {
    if blob.len() != BLOB_SIZE {
        return Err(ObjectError::Invalid(format!("blob is {} bytes, expected {BLOB_SIZE}", blob.len())));
    }
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(Nonce::from_slice(&nonce(kind, index)), Payload { msg: blob, aad: AAD })
        .map_err(|_| ObjectError::Invalid("blob does not open with this key".into()))
}

fn check_size(size: u64) -> Result<(), ObjectError> {
    if size == 0 {
        return Err(ObjectError::Invalid("empty attachment".into()));
    }
    if size > MAX_ATTACHMENT_SIZE {
        return Err(ObjectError::Invalid(format!("attachment is {size} bytes; limit is {MAX_ATTACHMENT_SIZE}")));
    }
    Ok(())
}

/// Seal a file under a fresh random key.
pub fn seal(data: &[u8]) -> Result<Sealed, ObjectError> {
    seal_with_key(data, mesh_crypto::random_bytes())
}

/// Seal under a given key. Never reuse a key for two different files.
pub fn seal_with_key(data: &[u8], key: [u8; 32]) -> Result<Sealed, ObjectError> {
    check_size(data.len() as u64)?;
    let mut blobs = Vec::with_capacity(1 + data.len().div_ceil(CHUNK_PLAINTEXT));
    blobs.push(Vec::new()); // manifest, filled in below
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; CHUNK_PLAINTEXT];
    for (i, piece) in data.chunks(CHUNK_PLAINTEXT).enumerate() {
        buf[..piece.len()].copy_from_slice(piece);
        buf[piece.len()..].fill(0);
        let blob = encrypt(&key, NONCE_CHUNK, i as u64, &buf);
        chunks.push(Digest::of(&blob));
        blobs.push(blob);
    }
    let manifest = Manifest { size: data.len() as u64, content: Digest::of(data), chunks };
    let mut w = Writer::new();
    w.raw(MANIFEST_MAGIC)
        .u8(MANIFEST_VERSION)
        .u64(manifest.size)
        .raw(&manifest.content.to_bytes())
        .u32(manifest.chunks.len() as u32);
    for c in &manifest.chunks {
        w.raw(&c.to_bytes());
    }
    let mut plain = w.finish();
    plain.resize(CHUNK_PLAINTEXT, 0);
    blobs[0] = encrypt(&key, NONCE_MANIFEST, 0, &plain);
    Ok(Sealed { manifest_id: Digest::of(&blobs[0]), key, size: manifest.size, blobs })
}

/// Decrypt and validate a manifest blob. The caller has already checked the
/// blob hashes to the manifest ID in the post.
pub fn open_manifest(blob: &[u8], key: &[u8; 32]) -> Result<Manifest, ObjectError> {
    let plain = decrypt(key, NONCE_MANIFEST, 0, blob)?;
    let mut r = Reader::new(&plain);
    if r.take(4)? != MANIFEST_MAGIC {
        return Err(ObjectError::Invalid("not a manifest".into()));
    }
    let version = r.u8()?;
    if version != MANIFEST_VERSION {
        return Err(ObjectError::Invalid(format!("unsupported manifest version {version}")));
    }
    let size = r.u64()?;
    check_size(size)?;
    let content = Digest::from_bytes(r.take(DIGEST_ENCODED_LEN)?)?;
    let n = r.u32()? as usize;
    if n != (size as usize).div_ceil(CHUNK_PLAINTEXT) {
        return Err(ObjectError::Invalid("chunk count does not match size".into()));
    }
    let mut chunks = Vec::with_capacity(n);
    for _ in 0..n {
        chunks.push(Digest::from_bytes(r.take(DIGEST_ENCODED_LEN)?)?);
    }
    if r.take(r.remaining())?.iter().any(|b| *b != 0) {
        return Err(ObjectError::Invalid("manifest padding is not zero".into()));
    }
    Ok(Manifest { size, content, chunks })
}

/// Decrypt chunk `index` of `manifest`, returning just the file bytes.
pub fn open_chunk(blob: &[u8], key: &[u8; 32], manifest: &Manifest, index: usize) -> Result<Vec<u8>, ObjectError> {
    let mut plain = decrypt(key, NONCE_CHUNK, index as u64, blob)?;
    let len = manifest.chunk_len(index);
    if plain[len..].iter().any(|b| *b != 0) {
        return Err(ObjectError::Invalid("chunk padding is not zero".into()));
    }
    plain.truncate(len);
    Ok(plain)
}

/// Guess a media type from a file name. Unknown types are `application/octet-stream`.
pub fn guess_media_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "odt" => "application/vnd.oasis.opendocument.text",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(len: usize) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
    }

    fn reassemble(s: &Sealed) -> Vec<u8> {
        let m = open_manifest(&s.blobs[0], &s.key).unwrap();
        let mut out = Vec::new();
        for (i, b) in s.blobs[1..].iter().enumerate() {
            assert_eq!(Digest::of(b), m.chunks[i]);
            out.extend(open_chunk(b, &s.key, &m, i).unwrap());
        }
        assert_eq!(Digest::of(&out), m.content);
        out
    }

    #[test]
    fn seal_and_open() {
        for len in [1, CHUNK_PLAINTEXT - 1, CHUNK_PLAINTEXT, CHUNK_PLAINTEXT + 1, 300_000] {
            let data = file(len);
            let s = seal(&data).unwrap();
            assert_eq!(s.blobs.len(), 1 + len.div_ceil(CHUNK_PLAINTEXT));
            assert_eq!(reassemble(&s), data);
        }
    }

    #[test]
    fn every_blob_is_uniform_and_opaque() {
        let data = b"Senate submission draft".repeat(5000);
        let s = seal(&data).unwrap();
        for b in &s.blobs {
            assert_eq!(b.len(), BLOB_SIZE, "manifest and chunks are indistinguishable by size");
            assert!(!b.windows(6).any(|w| w == b"Senate"), "plaintext visible in blob");
            assert!(!b.windows(4).any(|w| w == MANIFEST_MAGIC), "manifest recognisable");
        }
    }

    #[test]
    fn same_file_sealed_twice_is_unlinkable() {
        let data = file(100_000);
        let (a, b) = (seal(&data).unwrap(), seal(&data).unwrap());
        assert_ne!(a.manifest_id, b.manifest_id);
        assert!(a.blobs.iter().all(|x| !b.blobs.contains(x)), "no blob in common");
    }

    #[test]
    fn wrong_key_or_position_fails() {
        let s = seal(&file(200_000)).unwrap();
        assert!(open_manifest(&s.blobs[0], &[0u8; 32]).is_err());
        assert!(open_manifest(&s.blobs[1], &s.key).is_err(), "a chunk is not a manifest");
        let m = open_manifest(&s.blobs[0], &s.key).unwrap();
        assert!(open_chunk(&s.blobs[2], &s.key, &m, 0).is_err(), "chunks can't be reordered");
        let mut bad = s.blobs[1].clone();
        bad[100] ^= 1;
        assert!(open_chunk(&bad, &s.key, &m, 0).is_err());
    }

    #[test]
    fn limits() {
        assert!(seal(b"").is_err());
        assert_eq!(MAX_CHUNKS, 1983);
    }
}
