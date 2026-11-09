//! Immutable, content-addressed protocol objects (README §5–§10, SPEC.md §3).
//!
//! The only way to obtain a [`VerifiedObject`] is [`create`] (local authoring)
//! or [`VerifiedObject::decode`] (bytes from anywhere). Both run the same
//! validation, so code holding a `VerifiedObject` can rely on it being
//! well-formed, within limits, and correctly signed if it claims an author.

pub mod blob;
pub mod codec;
pub mod strip;

use codec::{DecodeError, Reader, Writer};
use mesh_crypto::{signing_input, CryptoError, Digest, Keypair, PublicKey, SIGNATURE_LEN};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAGIC: &[u8; 4] = b"SMSH";
pub const FORMAT_VERSION: u8 = 1;
pub const SIGNATURE_CONTEXT: &str = "socialmesh v1 object";

/// Upper bound on a whole encoded object (not including attachment data,
/// which travels separately as chunks).
pub const MAX_OBJECT_SIZE: usize = 256 * 1024;
pub const MAX_KIND_LEN: usize = 64;
pub const MAX_MEDIA_TYPE_LEN: usize = 127;
pub const MAX_BODY_LEN: usize = 64 * 1024;
pub const MAX_ATTACHMENTS: usize = 64;

pub const KIND_POST: &str = "post";
pub const TEXT_PLAIN: &str = "text/plain";
pub const TEXT_MARKDOWN: &str = "text/markdown";

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("malformed object: {0}")]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("object is {0} bytes; limit is {MAX_OBJECT_SIZE}")]
    TooLarge(usize),
    #[error("invalid object: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// ID of the sealed manifest blob (SPEC.md §4).
    pub object: Digest,
    /// Key that opens the manifest and chunks. Only holders of the post have it;
    /// nodes caching the blobs see opaque, uniform 64 KiB pieces.
    pub key: [u8; 32],
    pub media_type: String,
    pub size: u64,
}

/// Decoded object fields. Construct via [`create`] / [`VerifiedObject::decode`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub kind: String,
    pub created_ms: u64,
    pub author: Option<PublicKey>,
    pub parent: Option<Digest>,
    pub body_type: String,
    pub body: String,
    pub attachments: Vec<Attachment>,
    pub signature: Option<[u8; SIGNATURE_LEN]>,
}

/// Content to publish, before an author is chosen.
#[derive(Clone, Debug)]
pub struct Content {
    pub kind: String,
    pub created_ms: u64,
    pub parent: Option<Digest>,
    pub body_type: String,
    pub body: String,
    pub attachments: Vec<Attachment>,
}

impl Content {
    pub fn post(body: impl Into<String>) -> Self {
        Content {
            kind: KIND_POST.into(),
            created_ms: now_ms(),
            parent: None,
            body_type: TEXT_PLAIN.into(),
            body: body.into(),
            attachments: Vec::new(),
        }
    }
    pub fn reply_to(mut self, parent: Digest) -> Self {
        self.parent = Some(parent);
        self
    }
    pub fn attach(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }
    pub fn markdown(mut self) -> Self {
        self.body_type = TEXT_MARKDOWN.into();
        self
    }
}

pub enum Author<'a> {
    /// Signed by a persistent, pseudonymous, or thread-scoped key.
    Signed(&'a Keypair),
    /// No author. Integrity comes from the content hash only (README §18).
    Ownerless,
}

/// An object whose encoding, limits and signature have been checked.
#[derive(Clone, Debug)]
pub struct VerifiedObject {
    object: Object,
    bytes: Vec<u8>,
    id: Digest,
}

impl VerifiedObject {
    /// Decode and fully verify bytes received from any source.
    pub fn decode(bytes: &[u8]) -> Result<Self, ObjectError> {
        if bytes.len() > MAX_OBJECT_SIZE {
            return Err(ObjectError::TooLarge(bytes.len()));
        }
        let mut r = Reader::new(bytes);
        if r.take(4)? != MAGIC {
            return Err(ObjectError::Invalid("bad magic".into()));
        }
        let version = r.u8()?;
        if version != FORMAT_VERSION {
            return Err(ObjectError::Invalid(format!("unsupported format version {version}")));
        }
        let kind = r.str("kind", MAX_KIND_LEN)?.to_owned();
        let created_ms = r.u64()?;
        let author = if r.flag()? {
            Some(PublicKey::from_bytes(&r.fixed::<32>()?)?)
        } else {
            None
        };
        let parent = if r.flag()? {
            Some(Digest::from_bytes(r.take(mesh_crypto::DIGEST_ENCODED_LEN)?)?)
        } else {
            None
        };
        let body_type = r.str("body_type", MAX_MEDIA_TYPE_LEN)?.to_owned();
        let body = r.str("body", MAX_BODY_LEN)?.to_owned();
        let n = r.u16()? as usize;
        if n > MAX_ATTACHMENTS {
            return Err(DecodeError::TooLong { field: "attachments", max: MAX_ATTACHMENTS }.into());
        }
        let mut attachments = Vec::with_capacity(n);
        for _ in 0..n {
            attachments.push(Attachment {
                object: Digest::from_bytes(r.take(mesh_crypto::DIGEST_ENCODED_LEN)?)?,
                key: r.fixed()?,
                media_type: r.str("media_type", MAX_MEDIA_TYPE_LEN)?.to_owned(),
                size: r.u64()?,
            });
        }
        let signature = if r.flag()? { Some(r.fixed::<SIGNATURE_LEN>()?) } else { None };
        r.finish()?;

        let object = Object { kind, created_ms, author, parent, body_type, body, attachments, signature };
        object.validate_fields()?;
        object.verify_signature()?;

        // Canonical check: re-encoding must reproduce the input exactly.
        let reencoded = object.encode();
        if reencoded != bytes {
            return Err(ObjectError::Invalid("non-canonical encoding".into()));
        }
        Ok(VerifiedObject { id: Digest::of(bytes), bytes: reencoded, object })
    }

    pub fn id(&self) -> Digest {
        self.id
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn object(&self) -> &Object {
        &self.object
    }
}

/// Author and seal a new object.
pub fn create(content: Content, author: Author<'_>) -> Result<VerifiedObject, ObjectError> {
    let mut object = Object {
        kind: content.kind,
        created_ms: content.created_ms,
        author: match &author {
            Author::Signed(k) => Some(k.public()),
            Author::Ownerless => None,
        },
        parent: content.parent,
        body_type: content.body_type,
        body: content.body,
        attachments: content.attachments,
        signature: None,
    };
    object.validate_fields()?;
    if let Author::Signed(k) = author {
        object.signature = Some(k.sign(&signing_input(SIGNATURE_CONTEXT, &object.unsigned_bytes())));
    }
    let bytes = object.encode();
    // Run the exact receive path so local and remote objects are held to one standard.
    let verified = VerifiedObject::decode(&bytes)?;
    Ok(verified)
}

impl Object {
    /// The canonical encoding of every field except the signature.
    pub fn unsigned_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(MAGIC).u8(FORMAT_VERSION).str(&self.kind).u64(self.created_ms);
        w.flag(self.author.is_some());
        if let Some(a) = &self.author {
            w.raw(&a.to_bytes());
        }
        w.flag(self.parent.is_some());
        if let Some(p) = &self.parent {
            w.raw(&p.to_bytes());
        }
        w.str(&self.body_type).str(&self.body);
        w.u16(self.attachments.len() as u16);
        for a in &self.attachments {
            w.raw(&a.object.to_bytes()).raw(&a.key).str(&a.media_type).u64(a.size);
        }
        w.finish()
    }

    /// Full canonical encoding: unsigned bytes followed by the optional signature.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(&self.unsigned_bytes()).flag(self.signature.is_some());
        if let Some(s) = &self.signature {
            w.raw(s);
        }
        w.finish()
    }

    pub fn is_ownerless(&self) -> bool {
        self.author.is_none()
    }

    fn validate_fields(&self) -> Result<(), ObjectError> {
        let bad = |m: &str| Err(ObjectError::Invalid(m.into()));
        if self.kind.is_empty()
            || self.kind.len() > MAX_KIND_LEN
            || !self.kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return bad("kind must be 1-64 chars of [a-z0-9._-]");
        }
        if !valid_media_type(&self.body_type) {
            return bad("invalid body media type");
        }
        if self.body.len() > MAX_BODY_LEN {
            return bad("body too long");
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return bad("too many attachments");
        }
        if self.attachments.iter().any(|a| !valid_media_type(&a.media_type)) {
            return bad("invalid attachment media type");
        }
        if self.attachments.iter().any(|a| a.size == 0 || a.size > blob::MAX_ATTACHMENT_SIZE) {
            return bad("invalid attachment size");
        }
        let size = self.encode().len();
        if size > MAX_OBJECT_SIZE {
            return Err(ObjectError::TooLarge(size));
        }
        Ok(())
    }

    fn verify_signature(&self) -> Result<(), ObjectError> {
        match (&self.author, &self.signature) {
            (None, None) => Ok(()),
            (Some(pk), Some(sig)) => {
                pk.verify(&signing_input(SIGNATURE_CONTEXT, &self.unsigned_bytes()), sig)?;
                Ok(())
            }
            (Some(_), None) => Err(ObjectError::Invalid("authored object is missing its signature".into())),
            (None, Some(_)) => Err(ObjectError::Invalid("ownerless object carries a signature".into())),
        }
    }
}

fn valid_media_type(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_MEDIA_TYPE_LEN
        && s.contains('/')
        && s.bytes().all(|b| b.is_ascii_graphic())
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_roundtrip() {
        let k = Keypair::generate();
        let o = create(Content::post("Hello distributed world"), Author::Signed(&k)).unwrap();
        let again = VerifiedObject::decode(o.bytes()).unwrap();
        assert_eq!(again.id(), o.id());
        assert_eq!(again.object().author, Some(k.public()));
        assert_eq!(again.object().body, "Hello distributed world");
    }

    #[test]
    fn ownerless_roundtrip() {
        let o = create(Content::post("anon"), Author::Ownerless).unwrap();
        assert!(o.object().is_ownerless());
        assert!(VerifiedObject::decode(o.bytes()).is_ok());
    }

    #[test]
    fn deterministic_ids() {
        let k = Keypair::from_seed(&[7u8; 32]);
        let mut c = Content::post("same");
        c.created_ms = 1_700_000_000_000;
        let a = create(c.clone(), Author::Signed(&k)).unwrap();
        let b = create(c, Author::Signed(&k)).unwrap();
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn tampered_body_fails_signature() {
        let k = Keypair::generate();
        let o = create(Content::post("original"), Author::Signed(&k)).unwrap();
        let mut bytes = o.bytes().to_vec();
        let pos = bytes.windows(8).position(|w| w == b"original").unwrap();
        bytes[pos] = b'O';
        assert!(matches!(VerifiedObject::decode(&bytes), Err(ObjectError::Crypto(_))));
    }

    #[test]
    fn stripping_signature_is_rejected() {
        let k = Keypair::generate();
        let o = create(Content::post("x"), Author::Signed(&k)).unwrap();
        let mut obj = o.object().clone();
        obj.signature = None;
        assert!(VerifiedObject::decode(&obj.encode()).is_err());
    }

    #[test]
    fn rejects_trailing_bytes_and_truncation() {
        let o = create(Content::post("x"), Author::Ownerless).unwrap();
        let mut long = o.bytes().to_vec();
        long.push(0);
        assert!(VerifiedObject::decode(&long).is_err());
        let short = &o.bytes()[..o.bytes().len() - 1];
        assert!(VerifiedObject::decode(short).is_err());
    }

    #[test]
    fn rejects_oversize_body() {
        let body = "a".repeat(MAX_BODY_LEN + 1);
        assert!(create(Content::post(body), Author::Ownerless).is_err());
    }

    #[test]
    fn reply_references_parent() {
        let root = create(Content::post("root"), Author::Ownerless).unwrap();
        let reply = create(Content::post("re").reply_to(root.id()), Author::Ownerless).unwrap();
        assert_eq!(reply.object().parent, Some(root.id()));
    }
}
