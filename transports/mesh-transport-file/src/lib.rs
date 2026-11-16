//! Offline bundles (README §20).
//!
//! A bundle is just another way to move objects between devices: USB stick,
//! SD card, email attachment, Bluetooth file share. Imported objects go
//! through exactly the same verification as network-received ones.
//!
//! Format (SPEC.md §6):
//! `"SMBUNDLE" || u8 version || u32 count || count × (u32 len || object bytes)`,
//! then in version 2 `u32 blob_count || blob_count × blob`: the sealed pieces
//! of the objects' attachments (photos, site files), so they travel too.

use anyhow::{bail, Context, Result};
use mesh_object::blob::BLOB_SIZE;
use mesh_object::{VerifiedObject, MAX_OBJECT_SIZE};
use mesh_store::{BlobSource, Origin, Store};
use std::io::{Read, Write};

pub const BUNDLE_MAGIC: &[u8; 8] = b"SMBUNDLE";
/// Objects only (sound and other datagram media).
pub const BUNDLE_VERSION: u8 = 1;
/// Objects and their attachments' blobs.
pub const BUNDLE_VERSION_FILES: u8 = 2;
/// Refuse absurd bundles before reading them.
pub const MAX_BUNDLE_OBJECTS: u32 = 1_000_000;
pub const MAX_BUNDLE_BLOBS: u32 = 1_000_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub objects: usize,
    pub blobs: usize,
}

/// Write every object in `store` that `relay_enabled` permits sharing, and,
/// with `files`, every blob of their attachments that is held here and
/// shareable. Attachments only partly here travel partly; the receiver
/// fetches the rest like any missing piece.
pub fn export(store: &Store, relay_enabled: bool, files: bool, out: &mut impl Write) -> Result<ExportReport> {
    let ids = store.shareable_ids(relay_enabled)?;
    out.write_all(BUNDLE_MAGIC)?;
    out.write_all(&[if files { BUNDLE_VERSION_FILES } else { BUNDLE_VERSION }])?;
    out.write_all(&(ids.len() as u32).to_be_bytes())?;
    let mut blobs = Vec::new();
    for id in &ids {
        let obj = store.get(id)?.context("indexed object vanished")?;
        out.write_all(&(obj.bytes().len() as u32).to_be_bytes())?;
        out.write_all(obj.bytes())?;
        if files {
            for a in &obj.object().attachments {
                let Some(m) = store.manifest(&a.object, &a.key)? else { continue };
                for b in std::iter::once(a.object).chain(m.chunks) {
                    if !blobs.contains(&b) && store.has_blob(&b)? && store.blob_shareable(&b, relay_enabled)? {
                        blobs.push(b);
                    }
                }
            }
        }
    }
    if files {
        out.write_all(&(blobs.len() as u32).to_be_bytes())?;
        for b in &blobs {
            out.write_all(&store.get_blob(b)?.context("blob vanished")?)?;
        }
    }
    out.flush()?;
    Ok(ExportReport { objects: ids.len(), blobs: blobs.len() })
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
    /// Attachment pieces kept (version 2 bundles).
    pub blobs: usize,
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
    if v[0] != BUNDLE_VERSION && v[0] != BUNDLE_VERSION_FILES {
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
    if v[0] == BUNDLE_VERSION_FILES {
        let n = read_u32(input)?;
        if n > MAX_BUNDLE_BLOBS {
            bail!("bundle claims {n} blobs; limit is {MAX_BUNDLE_BLOBS}");
        }
        let mut buf = vec![0u8; BLOB_SIZE];
        for _ in 0..n {
            input.read_exact(&mut buf)?;
            // Named by its hash, so it can only be what it claims to be. Held
            // as cache, exactly as if a peer had sent it; the cache budget
            // applies.
            store.put_blob(&buf, BlobSource::Cache)?;
            report.blobs += 1;
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
        assert_eq!(export(&a, false, false, &mut buf).unwrap(), ExportReport { objects: 2, blobs: 0 });
        let r = import(&b, &mut buf.as_slice()).unwrap();
        assert_eq!(r, ImportReport { new: 2, duplicate: 0, rejected: 0, blobs: 0 });
        assert_eq!(b.children(&root.id()).unwrap()[0].id(), reply.id());

        let r = import(&b, &mut buf.as_slice()).unwrap();
        assert_eq!(r.duplicate, 2);
    }

    #[test]
    fn attachments_travel_in_a_version_2_bundle() {
        let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (Store::open(a_dir.path()).unwrap(), Store::open(b_dir.path()).unwrap());
        let data: Vec<u8> = (0..150_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let w = a.add_file(&data, BlobSource::Own).unwrap();
        let att = mesh_object::Attachment { object: w.manifest, key: w.key, media_type: "image/png".into(), size: w.size };
        let post = create(Content::post("photo").attach(att), Author::Signed(&Keypair::generate())).unwrap();
        a.put(&post, &Origin::Local).unwrap();

        let mut buf = Vec::new();
        let e = export(&a, false, true, &mut buf).unwrap();
        assert_eq!(e, ExportReport { objects: 1, blobs: 4 }, "manifest + 3 chunks");
        let r = import(&b, &mut buf.as_slice()).unwrap();
        assert_eq!((r.new, r.blobs), (1, 4));
        let mut out = Vec::new();
        b.write_attachment(&(&post.object().attachments[0]).into(), &mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn corrupted_entry_is_rejected_not_fatal() {
        let a_dir = tempfile::tempdir().unwrap();
        let a = Store::open(a_dir.path()).unwrap();
        a.put(&create(Content::post("x"), Author::Ownerless).unwrap(), &Origin::Local).unwrap();
        let mut buf = Vec::new();
        export(&a, false, false, &mut buf).unwrap();
        // Flip a byte inside the body text so the content no longer matches its canonical form.
        let pos = buf.iter().rposition(|&b| b == b'x').unwrap();
        buf[pos] = 0xff; // invalid UTF-8
        let b_dir = tempfile::tempdir().unwrap();
        let b = Store::open(b_dir.path()).unwrap();
        assert_eq!(import(&b, &mut buf.as_slice()).unwrap().rejected, 1);
    }
}
