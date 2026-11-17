//! Apps (SPEC.md §11): sites with code. The code runs in a web worker with
//! no network; what people send it are `app.entry` objects, replies to the
//! app, signed by default with a key unique to the person and the app.
//!
//! This module holds what both viewers (desktop and phone) need: the
//! entries, the per-app key, the security policies, and the host script
//! that is the only code ever run in an app's page.

use crate::view::{headers, neutralise_html};
use crate::{Address, Site};
use anyhow::{bail, Result};
use mesh_crypto::{Digest, Keypair};
use mesh_object::site::{SiteManifest, APP_ENTRY_BODY_TYPE, KIND_APP_ENTRY, KIND_SITE, MAX_ENTRY_BYTES};
use mesh_object::{now_ms, Content};
use mesh_store::Store;
use std::fmt::Write as _;

/// The only script that runs in an app's page.
pub const HOST_JS: &str = include_str!("../assets/host.js");
/// Starts app code in its worker and gives it the `socialmesh` API.
pub const WORKER_JS: &str = include_str!("../assets/worker.js");
/// Paths under this belong to the viewer, never to the site.
pub const RESERVED: &str = ".socialmesh/";

/// Policy for an app's worker and the scripts it imports: the site's own
/// scripts, and nothing else at all (no connections, no nested workers).
pub const WORKER_CSP: &str = "default-src 'none'; script-src 'self'; worker-src 'none'; child-src 'none'";

/// Policy for an app's pages: only the viewer's host script (by nonce)
/// runs; the worker comes from the site; nothing loads from outside.
pub fn page_csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; worker-src 'self'; connect-src 'self'; \
         img-src 'self' data: blob:; media-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-src 'none'; frame-ancestors 'none'"
    )
}

/// A fresh nonce for one page.
pub fn nonce() -> String {
    mesh_crypto::random_bytes::<16>().iter().map(|b| format!("{b:02x}")).collect()
}

fn replace_csp(mut h: Vec<(&'static str, String)>, csp: String) -> Vec<(&'static str, String)> {
    for (k, v) in &mut h {
        if *k == "Content-Security-Policy" {
            *v = csp.clone();
        }
    }
    h.retain(|(k, _)| *k != "Content-Disposition");
    h
}

/// Headers for an app's HTML page.
pub fn page_headers(media_type: &str, nonce: &str) -> Vec<(&'static str, String)> {
    replace_csp(headers(media_type), page_csp(nonce))
}

/// Headers for a script an app's worker loads.
pub fn script_headers() -> Vec<(&'static str, String)> {
    replace_csp(headers("text/javascript"), WORKER_CSP.to_string())
}

/// An app page ready to show: neutralised, with the host script inserted
/// first in `<head>` (or at the top). `page` is the page's site path, for
/// relative links to the viewer's scripts.
pub fn prepare_page(html: Vec<u8>, page: &str, app: &str, nonce: &str) -> Vec<u8> {
    let html = neutralise_html(html);
    let root = "../".repeat(page.matches('/').count());
    let tag = format!(
        r#"<script nonce="{nonce}" src="{root}{RESERVED}host.js" data-root="{root}" data-app="{app}"></script>"#
    );
    let lower = html.to_ascii_lowercase();
    let find_tag_end = |name: &[u8]| -> Option<usize> {
        let at = lower.windows(name.len()).position(|w| w == name)?;
        lower[at..].iter().position(|&b| b == b'>').map(|e| at + e + 1)
    };
    let at = find_tag_end(b"<head").or_else(|| find_tag_end(b"<!doctype")).unwrap_or(0);
    let mut out = Vec::with_capacity(html.len() + tag.len());
    out.extend_from_slice(&html[..at]);
    out.extend_from_slice(tag.as_bytes());
    out.extend_from_slice(&html[at..]);
    out
}

/// The key that signs this person's anonymous entries to this app: the same
/// for every entry to one app, unlinkable to them or to other apps.
pub fn app_key(anonymous_secret: &[u8; 32], address: &Address) -> Keypair {
    Keypair::derive_thread_key(anonymous_secret, &address.digest())
}

/// One entry sent to an app.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: Digest,
    /// Identity ID of the signer, if signed.
    pub author: Option<Digest>,
    pub created_ms: u64,
    pub data: String,
}

/// Every version of the site at `address` held here.
fn versions(store: &Store, address: &Address) -> Result<Vec<Digest>> {
    let mut out = Vec::new();
    for o in store.by_kind(KIND_SITE)? {
        let here = match (address, &o.object().author) {
            (Address::Keyed(k), Some(a)) => a.id() == *k,
            (Address::Ownerless(id), None) => o.id() == *id,
            _ => false,
        };
        if here && matches!(SiteManifest::of(o.object()), Some(Ok(_))) {
            out.push(o.id());
        }
    }
    Ok(out)
}

/// Entries to every version of `site`, oldest first.
pub fn entries(store: &Store, site: &Site) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for v in versions(store, &site.address)? {
        for c in store.children(&v)? {
            let o = c.object();
            if o.kind == KIND_APP_ENTRY
                && o.body_type == APP_ENTRY_BODY_TYPE
                && o.body.len() <= MAX_ENTRY_BYTES
                && o.attachments.is_empty()
            {
                out.push(Entry { id: c.id(), author: o.author.as_ref().map(|a| a.id()), created_ms: o.created_ms, data: o.body.clone() });
            }
        }
    }
    out.sort_by_key(|e| (e.created_ms, e.id.to_bytes()));
    Ok(out)
}

/// Content for a new entry to `site` (the caller signs it).
pub fn entry_content(site: &Site, data: &str) -> Result<Content> {
    if site.manifest.app.is_none() {
        bail!("this site is not an app");
    }
    if data.len() > MAX_ENTRY_BYTES {
        bail!("an entry can be at most {MAX_ENTRY_BYTES} bytes");
    }
    Ok(Content {
        kind: KIND_APP_ENTRY.into(),
        created_ms: now_ms(),
        parent: Some(site.object.id()),
        body_type: APP_ENTRY_BODY_TYPE.into(),
        body: data.to_string(),
        attachments: Vec::new(),
    })
}

/// JSON string literal.
pub fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `{"title","address","version"}` for the app's code.
pub fn app_json(site: &Site) -> String {
    let mut out = String::from("{\"title\":");
    json_str(&mut out, &site.manifest.title);
    let _ = write!(out, ",\"address\":\"{}\",\"version\":{}}}", site.address, site.manifest.version);
    out
}

/// Entries as the app sees them: `[{"id","author","mine","created_ms","data"}]`.
/// `mine` holds the identity IDs that count as this person (their app key
/// and posting identity).
pub fn entries_json(store: &Store, site: &Site, mine: &[Digest]) -> Result<String> {
    let mut out = String::from("[");
    for (i, e) in entries(store, site)?.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"id\":\"{}\",\"author\":", e.id.to_hex());
        match e.author {
            Some(a) => {
                let _ = write!(out, "\"{}\"", a.short());
            }
            None => out.push_str("null"),
        }
        let _ = write!(out, ",\"mine\":{},\"created_ms\":{},\"data\":", e.author.is_some_and(|a| mine.contains(&a)), e.created_ms);
        json_str(&mut out, &e.data);
        out.push('}');
    }
    out.push(']');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_app, latest, NewFile};
    use mesh_object::{create, Author};
    use mesh_store::{BlobSource, Origin};

    fn file(path: &str, data: &str) -> NewFile {
        NewFile { path: path.into(), media_type: mesh_object::blob::guess_media_type(path).into(), data: data.into() }
    }

    #[test]
    fn entries_follow_the_app_across_versions_and_anonymous_keys_are_per_app() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = Keypair::generate();
        let files = || vec![file("index.html", "<div data-socialmesh-app></div>"), file("poll.js", "socialmesh.render('hi')")];
        let c = build_app(&store, files(), "Poll", "index.html", Some("poll.js"), None, BlobSource::Own).unwrap();
        store.put(&create(c, Author::Signed(&key)).unwrap(), &Origin::Local).unwrap();
        let v1 = latest(&store, &Address::Keyed(key.public().id())).unwrap().unwrap();

        let secret = [7u8; 32];
        let me = app_key(&secret, &v1.address);
        store.put(&create(entry_content(&v1, r#"{"vote":"yes"}"#).unwrap(), Author::Signed(&me)).unwrap(), &Origin::Local).unwrap();

        // A new version: entries to the old one still count.
        let c = build_app(&store, files(), "Poll", "index.html", Some("poll.js"), Some(&v1), BlobSource::Own).unwrap();
        store.put(&create(c, Author::Signed(&key)).unwrap(), &Origin::Local).unwrap();
        let v2 = latest(&store, &v1.address).unwrap().unwrap();
        assert_eq!(v2.manifest.version, 2);
        let other = Keypair::generate();
        store.put(&create(entry_content(&v2, r#"{"vote":"no"}"#).unwrap(), Author::Signed(&other)).unwrap(), &Origin::Local).unwrap();

        let all = entries(&store, &v2).unwrap();
        assert_eq!(all.len(), 2);
        let json = entries_json(&store, &v2, &[me.public().id()]).unwrap();
        assert!(json.contains(r#""mine":true,"#) && json.contains(r#""data":"{\"vote\":\"yes\"}""#), "{json}");

        // The same person, another app: a different key.
        assert_eq!(app_key(&secret, &v1.address).public().id(), me.public().id());
        assert_ne!(app_key(&secret, &Address::Keyed(Keypair::generate().public().id())).public().id(), me.public().id());

        // Plain sites take no entries; entries have a size limit.
        let plain = crate::build(&store, vec![file("index.html", "x")], "Plain", "index.html", None, BlobSource::Own).unwrap();
        store.put(&create(plain, Author::Signed(&other)).unwrap(), &Origin::Local).unwrap();
        let plain = latest(&store, &Address::Keyed(other.public().id())).unwrap().unwrap();
        assert!(entry_content(&plain, "{}").is_err());
        assert!(entry_content(&v2, &"x".repeat(MAX_ENTRY_BYTES + 1)).is_err());
    }

    #[test]
    fn the_host_script_goes_first_with_its_nonce_and_relative_root() {
        let page = b"<!doctype html><html><HEAD><title>t</title></head><body><script>evil()</script>".to_vec();
        let out = String::from_utf8(prepare_page(page, "polls/today.html", "poll.js", "abc")).unwrap();
        assert!(
            out.contains(r#"<HEAD><script nonce="abc" src="../.socialmesh/host.js" data-root="../" data-app="poll.js"></script><title>"#),
            "{out}"
        );
        assert!(page_csp("abc").contains("script-src 'nonce-abc';"));
        let bare = String::from_utf8(prepare_page(b"<p>hi".to_vec(), "index.html", "a.js", "n")).unwrap();
        assert!(bare.starts_with(r#"<script nonce="n" src=".socialmesh/host.js""#), "{bare}");
    }
}
