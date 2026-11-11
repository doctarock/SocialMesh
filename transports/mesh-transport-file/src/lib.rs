//! Offline bundles (README §20).
//!
//! A bundle is just another way to move objects between devices: USB stick,
//! SD card, email attachment, Bluetooth file share. Imported objects go
//! through exactly the same verification as network-received ones.
//!
//! Format (SPEC.md §6):
//! `"SMBUNDLE" || u8 version=1 || u32 count || count × (u32 len || object bytes)`

use anyhow::{bail, Context, Result};
use mesh_object::{VerifiedObject, MAX_OBJECT_SIZE};
use mesh_store::{Origin, Store};
use std::io::{Read, Write};

pub const BUNDLE_MAGIC: &[u8; 8] = b"SMBUNDLE";
pub const BUNDLE_VERSION: u8 = 1;
/// Refuse absurd bundles before reading them.
pub const MAX_BUNDLE_OBJECTS: u32 = 1_000_000;

/// Write every object in `store` that `relay_enabled` permits sharing.
pub fn export(store: &Store, relay_enabled: bool, out: &mut impl Write) -> Result<usize> {
    let ids = store.shareable_ids(relay_enabled)?;
    out.write_all(BUNDLE_MAGIC)?;
    out.write_all(&[BUNDLE_VERSION])?;
    out.write_all(&(ids.len() as u32).to_be_bytes())?;
    for id in &ids {
        let obj = store.get(id)?.context("indexed object vanished")?;
        out.write_all(&(obj.bytes().len() as u32).to_be_bytes())?;
        out.write_all(obj.bytes())?;
    }
    out.flush()?;
    Ok(ids.len())
}

/// A bundle holding exactly `objects`, in memory (for audio and other
/// datagram media, which carry a bundle as their payload).
pub fn bundle(objects: &[VerifiedObject]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(BUNDLE_MAGIC);
    out.push(BUNDLE_VERSION);
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for o in objects {
        out.extend_from_slice(&(o.bytes().len() as u32).to_be_bytes());
        out.extend_from_slice(o.bytes());
    }
    out
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub new: usize,
    pub duplicate: usize,
    pub rejected: usize,
}

/// Verify and retain each object in a bundle. Invalid objects are skipped,
/// not fatal: one bad object should not poison a whole sneakernet transfer.
pub fn import(store: &Store, input: &mut impl Read) -> Result<ImportReport> {
    let mut magic = [0u8; 8];
    input.read_exact(&mut magic).context("reading bundle header")?;
    if &magic != BUNDLE_MAGIC {
        bail!("not a SocialMesh bundle");
    }
    let mut v = [0u8; 1];
    input.read_exact(&mut v)?;
    if v[0] != BUNDLE_VERSION {
        bail!("unsupported bundle version {}", v[0]);
    }
    let count = read_u32(input)?;
    if count > MAX_BUNDLE_OBJECTS {
        bail!("bundle claims {count} objects; limit is {MAX_BUNDLE_OBJECTS}");
    }
    let mut report = ImportReport::default();
    for _ in 0..count {
        let len = read_u32(input)? as usize;
        if len > MAX_OBJECT_SIZE {
            bail!("bundle entry of {len} bytes exceeds object limit");
        }
        let mut buf = vec![0u8; len];
        input.read_exact(&mut buf)?;
        match VerifiedObject::decode(&buf) {
            Ok(obj) => {
                if store.put(&obj, &Origin::Import)? {
                    report.new += 1;
                } else {
                    report.duplicate += 1;
                }
            }
            Err(_) => report.rejected += 1,
        }
    }
    Ok(report)
}

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_crypto::Keypair;
    use mesh_object::{create, Author, Content};

    #[test]
    fn export_import_roundtrip() {
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let a = Store::open(a_dir.path()).unwrap();
        let b = Store::open(b_dir.path()).unwrap();
        let k = Keypair::generate();
        let root = create(Content::post("offline hello"), Author::Signed(&k)).unwrap();
        let reply = create(Content::post("reply").reply_to(root.id()), Author::Ownerless).unwrap();
        a.put(&root, &Origin::Local).unwrap();
        a.put(&reply, &Origin::Local).unwrap();

        let mut buf = Vec::new();
        assert_eq!(export(&a, false, &mut buf).unwrap(), 2);
        let r = import(&b, &mut buf.as_slice()).unwrap();
        assert_eq!(r, ImportReport { new: 2, duplicate: 0, rejected: 0 });
        assert_eq!(b.children(&root.id()).unwrap()[0].id(), reply.id());

        let r = import(&b, &mut buf.as_slice()).unwrap();
        assert_eq!(r.duplicate, 2);
    }

    #[test]
    fn corrupted_entry_is_rejected_not_fatal() {
        let a_dir = tempfile::tempdir().unwrap();
        let a = Store::open(a_dir.path()).unwrap();
        a.put(&create(Content::post("x"), Author::Ownerless).unwrap(), &Origin::Local).unwrap();
        let mut buf = Vec::new();
        export(&a, false, &mut buf).unwrap();
        // Flip a byte inside the body text so the content no longer matches its canonical form.
        let pos = buf.iter().rposition(|&b| b == b'x').unwrap();
        buf[pos] = 0xff; // invalid UTF-8
        let b_dir = tempfile::tempdir().unwrap();
        let b = Store::open(b_dir.path()).unwrap();
        assert_eq!(import(&b, &mut buf.as_slice()).unwrap().rejected, 1);
    }
}
