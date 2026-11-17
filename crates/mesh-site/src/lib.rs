//! Sites (SPEC.md §10): sets of files published as one object, passed around
//! like posts, shown by a viewer that runs no scripts and loads nothing from
//! outside the site.
//!
//! This crate works on a [`Store`]: listing the sites a node holds (newest
//! version of each), building a new version from files, and reading a site's
//! files back for viewing. [`view`] holds the viewing rules; [`serve`] is a
//! loopback web server for viewing in an ordinary browser.

use anyhow::{bail, Context, Result};
use mesh_crypto::Digest;
use mesh_object::site::{valid_path, SiteFile, SiteManifest, KIND_SITE, SITE_BODY_TYPE};
use mesh_object::{now_ms, Attachment, Content, VerifiedObject};
use mesh_store::{BlobSource, Store, WantedAttachment};
use std::collections::HashMap;
use std::fmt;

pub mod app;
pub mod serve;
pub mod view;

/// Whole-site size a node accepts to publish (SPEC.md §10.5).
pub const MAX_SITE_BYTES: u64 = 32 * 1024 * 1024;

/// Where a site lives: its key (updatable) or, for an ownerless site, its
/// one object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Address {
    Keyed(Digest),
    Ownerless(Digest),
}

impl Address {
    pub fn digest(&self) -> Digest {
        match self {
            Address::Keyed(d) | Address::Ownerless(d) => *d,
        }
    }
    pub fn short(&self) -> String {
        self.digest().short()
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "site:{}", self.digest().to_hex())
    }
}

/// The version of a site a viewer shows.
#[derive(Clone, Debug)]
pub struct Site {
    pub address: Address,
    pub object: VerifiedObject,
    pub manifest: SiteManifest,
}

impl Site {
    pub fn attachment(&self, f: &SiteFile) -> &Attachment {
        &self.object.object().attachments[f.attachment]
    }
    pub fn total_size(&self) -> u64 {
        self.object.object().attachments.iter().map(|a| a.size).sum()
    }
    /// Files present and verified, out of all.
    pub fn downloaded(&self, store: &Store) -> Result<(usize, usize)> {
        let mut n = 0;
        for a in &self.object.object().attachments {
            if file_ready(store, a)? {
                n += 1;
            }
        }
        Ok((n, self.manifest.files.len()))
    }
}

fn file_ready(store: &Store, a: &Attachment) -> Result<bool> {
    Ok(match store.manifest(&a.object, &a.key)? {
        Some(m) => store.missing_chunks(&m)?.is_empty(),
        None => false,
    })
}

/// Every site this store holds, newest version of each, newest first.
/// Objects that claim to be sites but aren't valid are skipped.
pub fn sites(store: &Store) -> Result<Vec<Site>> {
    let mut best: HashMap<Address, Site> = HashMap::new();
    for object in store.by_kind(KIND_SITE)? {
        let Some(Ok(manifest)) = SiteManifest::of(object.object()) else { continue };
        let address = match &object.object().author {
            Some(k) => Address::Keyed(k.id()),
            None => Address::Ownerless(object.id()),
        };
        let candidate = Site { address, object, manifest };
        match best.get(&address) {
            // Highest version wins; on a tie, the smallest object ID (SPEC.md §10.3).
            Some(cur)
                if (cur.manifest.version, std::cmp::Reverse(cur.object.id().to_bytes()))
                    >= (candidate.manifest.version, std::cmp::Reverse(candidate.object.id().to_bytes())) => {}
            _ => {
                best.insert(address, candidate);
            }
        }
    }
    let mut out: Vec<Site> = best.into_values().collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.object.object().created_ms));
    Ok(out)
}

/// A site by `site:<hex>`, bare hex, or an unambiguous prefix of either the
/// full or short form.
pub fn find(store: &Store, text: &str) -> Result<Site> {
    let t = text.trim().trim_start_matches("site:").to_ascii_lowercase();
    if t.is_empty() {
        bail!("no site given");
    }
    let mut hits: Vec<Site> = sites(store)?
        .into_iter()
        .filter(|s| {
            let d = s.address.digest();
            d.to_hex().starts_with(&t) || d.short().to_ascii_lowercase().starts_with(&t)
        })
        .collect();
    match hits.len() {
        0 => bail!("no site matches `{text}` (see `mesh site list`)"),
        1 => Ok(hits.remove(0)),
        _ => bail!("`{text}` matches several sites; use more characters"),
    }
}

/// The newest version of the site at `address`, if held.
pub fn latest(store: &Store, address: &Address) -> Result<Option<Site>> {
    Ok(sites(store)?.into_iter().find(|s| s.address == *address))
}

/// One file to publish.
pub struct NewFile {
    pub path: String,
    pub media_type: String,
    pub data: Vec<u8>,
}

/// Content for a new version of a site (the caller signs it with the site
/// key, or leaves it ownerless). Files whose bytes match a file in
/// `previous` reuse its attachment, so only changed files travel (SPEC.md
/// §10.3). New files are sealed into the store as `source`.
pub fn build(
    store: &Store,
    files: Vec<NewFile>,
    title: &str,
    index: &str,
    previous: Option<&Site>,
    source: BlobSource,
) -> Result<Content> {
    build_app(store, files, title, index, None, previous, source)
}

/// [`build`] for an app: `app` names its script (SPEC.md §11.1), or `None`
/// for a plain site.
pub fn build_app(
    store: &Store,
    files: Vec<NewFile>,
    title: &str,
    index: &str,
    app: Option<&str>,
    previous: Option<&Site>,
    source: BlobSource,
) -> Result<Content> {
    if files.is_empty() {
        bail!("a site needs at least one file");
    }
    if files.len() > mesh_object::MAX_ATTACHMENTS {
        bail!("{} files; a site holds at most {}", files.len(), mesh_object::MAX_ATTACHMENTS);
    }
    let total: u64 = files.iter().map(|f| f.data.len() as u64).sum();
    if total > MAX_SITE_BYTES {
        bail!("site is {} bytes; the limit is {MAX_SITE_BYTES}", total);
    }
    for f in &files {
        if !valid_path(&f.path) {
            bail!("`{}` isn't a valid site path (letters, digits, . _ ~ - and / only)", f.path);
        }
    }
    // Unchanged files from the previous version, by content hash. Only for
    // `Own` blobs: a private-route publish must not reuse blobs marked as
    // this node's own (they would identify it as the source).
    let mut reusable: HashMap<Digest, Attachment> = HashMap::new();
    if let (Some(prev), BlobSource::Own) = (previous, source) {
        for a in &prev.object.object().attachments {
            if let Some(m) = store.manifest(&a.object, &a.key)? {
                if store.missing_chunks(&m)?.is_empty() {
                    reusable.insert(m.content, a.clone());
                }
            }
        }
    }
    let mut files = files;
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut attachments = Vec::with_capacity(files.len());
    let mut listed = Vec::with_capacity(files.len());
    for (i, f) in files.into_iter().enumerate() {
        let hash = Digest::of(&f.data);
        let att = match reusable.get(&hash) {
            Some(a) if a.media_type == f.media_type => a.clone(),
            _ => {
                let w = store.add_file(&f.data, source).with_context(|| format!("sealing {}", f.path))?;
                Attachment { object: w.manifest, key: w.key, media_type: f.media_type, size: w.size }
            }
        };
        attachments.push(att);
        listed.push(SiteFile { path: f.path, attachment: i });
    }
    let manifest = SiteManifest {
        version: previous.map_or(1, |p| p.manifest.version + 1),
        title: title.trim().to_string(),
        index: index.to_string(),
        app: app.map(str::to_string),
        files: listed,
    };
    manifest.check(attachments.len()).map_err(|e| anyhow::anyhow!("{}", e.0))?;
    Ok(Content {
        kind: KIND_SITE.into(),
        created_ms: now_ms(),
        parent: None,
        body_type: SITE_BODY_TYPE.into(),
        body: manifest.encode(),
        attachments,
    })
}

/// How many attachments of the site the new content reuses (for reporting).
pub fn reused(content: &Content, previous: Option<&Site>) -> usize {
    let Some(prev) = previous else { return 0 };
    let old: Vec<Digest> = prev.object.object().attachments.iter().map(|a| a.object).collect();
    content.attachments.iter().filter(|a| old.contains(&a.object)).count()
}

/// Queue every file of `site` for download.
pub fn want(store: &Store, site: &Site) -> Result<()> {
    for a in &site.object.object().attachments {
        store.want_attachment(&WantedAttachment::from(a))?;
    }
    Ok(())
}

/// A file looked up for viewing.
pub enum Lookup {
    Ready { data: Vec<u8>, media_type: String },
    /// Part of the site, not downloaded (or not verified) yet.
    NotYet,
    /// Not part of the site.
    Missing,
}

/// Read `path` from `site`. Only paths listed in the manifest are served.
pub fn read(store: &Store, site: &Site, path: &str) -> Result<Lookup> {
    let Some(f) = site.manifest.file(path) else { return Ok(Lookup::Missing) };
    let a = site.attachment(f);
    if !file_ready(store, a)? {
        return Ok(Lookup::NotYet);
    }
    let mut data = Vec::with_capacity(a.size as usize);
    match store.write_attachment(&WantedAttachment::from(a), &mut data) {
        Ok(_) => Ok(Lookup::Ready { data, media_type: a.media_type.clone() }),
        Err(mesh_store::StoreError::Incomplete(_)) => Ok(Lookup::NotYet),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_crypto::Keypair;
    use mesh_object::{create, Author};
    use mesh_store::Origin;

    fn file(path: &str, data: &str) -> NewFile {
        NewFile {
            path: path.into(),
            media_type: mesh_object::blob::guess_media_type(path).into(),
            data: data.as_bytes().to_vec(),
        }
    }

    fn publish(store: &Store, key: &Keypair, files: Vec<NewFile>, prev: Option<&Site>) -> Site {
        let c = build(store, files, "Pamphlet", "index.html", prev, BlobSource::Own).unwrap();
        let obj = create(c, Author::Signed(key)).unwrap();
        store.put(&obj, &Origin::Local).unwrap();
        latest(store, &Address::Keyed(key.public().id())).unwrap().unwrap()
    }

    #[test]
    fn versions_reuse_unchanged_files_and_the_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = Keypair::generate();
        let v1 = publish(&store, &key, vec![file("index.html", "<h1>one</h1>"), file("style.css", "h1{}")], None);
        assert_eq!(v1.manifest.version, 1);
        let v2 = publish(&store, &key, vec![file("index.html", "<h1>two</h1>"), file("style.css", "h1{}")], Some(&v1));
        assert_eq!(v2.manifest.version, 2);
        let css1 = v1.attachment(v1.manifest.file("style.css").unwrap()).object;
        let css2 = v2.attachment(v2.manifest.file("style.css").unwrap()).object;
        assert_eq!(css1, css2, "unchanged file reused");
        assert_ne!(v1.attachment(v1.manifest.file("index.html").unwrap()).object, v2.attachment(v2.manifest.file("index.html").unwrap()).object);

        let all = sites(&store).unwrap();
        assert_eq!(all.len(), 1, "one site, newest version");
        assert_eq!(all[0].manifest.version, 2);
        match read(&store, &all[0], "index.html").unwrap() {
            Lookup::Ready { data, media_type } => {
                assert_eq!(data, b"<h1>two</h1>");
                assert_eq!(media_type, "text/html");
            }
            _ => panic!("index not readable"),
        }
        assert!(matches!(read(&store, &all[0], "secret.txt").unwrap(), Lookup::Missing));
        let found = find(&store, &key.public().id().short()).unwrap();
        assert_eq!(found.address, Address::Keyed(key.public().id()));
    }

    #[test]
    fn someone_else_cannot_publish_a_version_of_your_site() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (mine, theirs) = (Keypair::generate(), Keypair::generate());
        let v1 = publish(&store, &mine, vec![file("index.html", "real")], None);
        // Another key copying the manifest with a higher version makes a different site.
        let mut c = build(&store, vec![file("index.html", "fake")], "Pamphlet", "index.html", Some(&v1), BlobSource::Own).unwrap();
        c.body = c.body.replace("version 2", "version 99");
        store.put(&create(c, Author::Signed(&theirs)).unwrap(), &Origin::Local).unwrap();
        let mine_now = latest(&store, &Address::Keyed(mine.public().id())).unwrap().unwrap();
        assert_eq!(mine_now.manifest.version, 1);
        assert_eq!(sites(&store).unwrap().len(), 2);
    }

    #[test]
    fn files_not_downloaded_are_reported_as_not_yet() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = Keypair::generate();
        let other = tempfile::tempdir().unwrap();
        let elsewhere = Store::open(other.path()).unwrap();
        // Built in another store: this one gets the object but no blobs.
        let c = build(&elsewhere, vec![file("index.html", "hi")], "T", "index.html", None, BlobSource::Own).unwrap();
        store.put(&create(c, Author::Signed(&key)).unwrap(), &Origin::Import).unwrap();
        let s = &sites(&store).unwrap()[0];
        assert!(matches!(read(&store, s, "index.html").unwrap(), Lookup::NotYet));
        assert_eq!(s.downloaded(&store).unwrap(), (0, 1));
    }

    #[test]
    fn bad_inputs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(build(&store, vec![], "T", "index.html", None, BlobSource::Own).is_err());
        assert!(build(&store, vec![file("../x.html", "")], "T", "../x.html", None, BlobSource::Own).is_err());
        assert!(build(&store, vec![file("a.html", "")], "T", "index.html", None, BlobSource::Own).is_err(), "index must exist");
        assert!(build(&store, vec![file("index.html", "")], "", "index.html", None, BlobSource::Own).is_err(), "title");
    }
}
