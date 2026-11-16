//! Publishing sites from the phone (SPEC.md §10). Site keys are stored like
//! the CLI's (`sites/<name>.key`, hex seed), so a home folder works with both.

use crate::{push_json_str, Mobile};
use anyhow::{bail, Result};
use mesh_crypto::Keypair;
use mesh_object::{create, Author};
use mesh_site::{Address, NewFile};
use mesh_store::BlobSource;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Largest anonymous site: every relay on its private route carries all of it.
const MAX_OWNERLESS_BYTES: u64 = 16 * 1024 * 1024;

/// Which site a publish goes to.
pub enum SiteTarget<'a> {
    /// A new site with a fresh key.
    New,
    /// A new version of the site whose key is stored under this name.
    Update(&'a str),
    /// No key: anonymous, can never be updated, always on the private route.
    Ownerless,
}

fn key_dir(m: &Mobile) -> PathBuf {
    m.home.join("sites")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn load_key(m: &Mobile, name: &str) -> Result<Keypair> {
    if !valid_name(name) {
        bail!("bad site key name");
    }
    let text = std::fs::read_to_string(key_dir(m).join(format!("{name}.key")))?;
    let raw = hex::decode(text.trim())?;
    let seed: [u8; 32] = raw.try_into().map_err(|_| anyhow::anyhow!("site key has the wrong length"))?;
    Ok(Keypair::from_seed(&seed))
}

/// A fresh key, named after the title (unique on this device).
fn new_key(m: &Mobile, title: &str) -> Result<(String, Keypair)> {
    let slug: String = title.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let mut base = slug.split('-').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("-");
    base.truncate(40);
    let base = if base.is_empty() { "site".to_string() } else { base.trim_end_matches('-').to_string() };
    std::fs::create_dir_all(key_dir(m))?;
    let mut name = base.clone();
    let mut n = 2;
    while key_dir(m).join(format!("{name}.key")).exists() {
        name = format!("{base}-{n}");
        n += 1;
    }
    let k = Keypair::generate();
    std::fs::write(key_dir(m).join(format!("{name}.key")), hex::encode(k.seed()))?;
    Ok((name, k))
}

/// Strip identifying metadata as posts do; refuse what can't be cleaned
/// unless `keep_metadata`. The error starts with `METADATA ` so the app can
/// ask the user.
fn clean(path: &str, media_type: &str, data: Vec<u8>, keep_metadata: bool) -> Result<Vec<u8>> {
    use mesh_object::strip::{strip_metadata, Strip};
    match strip_metadata(media_type, &data) {
        Ok(Strip::Cleaned { data, .. }) => Ok(data),
        Ok(Strip::NothingToStrip) => Ok(data),
        Ok(Strip::Unsupported) | Err(_) if keep_metadata => Ok(data),
        Ok(Strip::Unsupported) => {
            bail!("METADATA {path}: hidden details (creator, device, location) can't be removed from {media_type} files")
        }
        Err(e) => bail!("METADATA {path}: couldn't read it as {media_type} to remove hidden details ({e})"),
    }
}

impl Mobile {
    /// This device's site keys, as a JSON array of
    /// `{"key","address","short","title","version"}`; title and version are
    /// null until something is published (or arrives) under the key.
    pub fn my_sites_json(&self) -> Result<String> {
        let mut keys: Vec<(String, Address)> = match std::fs::read_dir(key_dir(self)) {
            Ok(dir) => dir
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().strip_suffix(".key")?.to_string();
                    let k = load_key(self, &name).ok()?;
                    Some((name, Address::Keyed(k.public().id())))
                })
                .collect(),
            Err(_) => vec![],
        };
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        let sites = self.node.with_store(mesh_site::sites)?;
        let mut out = String::from("[");
        for (i, (name, address)) in keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{{\"key\":\"{name}\",\"address\":\"{address}\",\"short\":\"{}\",\"title\":", address.short());
            match sites.iter().find(|s| s.address == *address) {
                Some(s) => {
                    push_json_str(&mut out, &s.manifest.title);
                    let _ = write!(out, ",\"version\":{}}}", s.manifest.version);
                }
                None => out.push_str("null,\"version\":null}"),
            }
        }
        out.push(']');
        Ok(out)
    }

    /// Publish files, given as (site path, bytes), as a site. Media types
    /// come from the names. Returns a line describing what was published.
    pub fn publish_site(
        &self,
        target: SiteTarget,
        title: &str,
        index: &str,
        files: Vec<(String, Vec<u8>)>,
        keep_metadata: bool,
    ) -> Result<String> {
        let mut new_files = Vec::with_capacity(files.len());
        for (path, data) in files {
            let media_type = mesh_object::blob::guess_media_type(&path);
            let data = clean(&path, media_type, data, keep_metadata)?;
            new_files.push(NewFile { path, media_type: media_type.into(), data });
        }
        let total: u64 = new_files.iter().map(|f| f.data.len() as u64).sum();
        let ownerless = matches!(target, SiteTarget::Ownerless);
        if ownerless && total > MAX_OWNERLESS_BYTES {
            bail!("an anonymous site can be at most 16 MB (every relay on its route carries all of it)");
        }
        let (key, new_name) = match target {
            SiteTarget::New => {
                let (name, k) = new_key(self, title)?;
                (Some(k), Some(name))
            }
            SiteTarget::Update(name) => (Some(load_key(self, name)?), None),
            SiteTarget::Ownerless => (None, None),
        };
        let result = self.publish_with(key.as_ref(), title, index, new_files, ownerless);
        if result.is_err() {
            // Don't leave behind a key for a site that was never published.
            if let Some(name) = new_name {
                let _ = std::fs::remove_file(key_dir(self).join(format!("{name}.key")));
            }
        }
        result
    }

    fn publish_with(&self, key: Option<&Keypair>, title: &str, index: &str, files: Vec<NewFile>, ownerless: bool) -> Result<String> {
        let previous = match key {
            Some(k) => self.node.with_store(|s| mesh_site::latest(s, &Address::Keyed(k.public().id())))?,
            None => None,
        };
        let source = if ownerless { BlobSource::Cache } else { BlobSource::Own };
        let content = self.node.with_store(|s| mesh_site::build(s, files, title, index, previous.as_ref(), source))?;
        let reused = mesh_site::reused(&content, previous.as_ref());
        let n = content.attachments.len();
        let version = previous.map_or(1, |p| p.manifest.version + 1);
        let address = match key {
            Some(k) => {
                self.node.publish(&create(content, Author::Signed(k))?)?;
                Address::Keyed(k.public().id())
            }
            None => {
                // Anonymous: coarse time, private route (as for anonymous posts).
                let mut c = content;
                c.created_ms -= c.created_ms % 60_000;
                let obj = create(c, Author::Ownerless)?;
                self.node.publish_anonymous(&obj)?;
                Address::Ownerless(obj.id())
            }
        };
        Ok(format!("Published \"{}\" version {version}: {n} files, {reused} unchanged ({})", title.trim(), address.short()))
    }
}
