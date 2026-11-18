//! Sites and apps for the phone app (SPEC.md §10, §11): list them, queue
//! their files, hand the viewer one file at a time under the same rules as
//! the desktop viewer, and carry out an app's requests.

use crate::{push_json_str, Mobile};
use anyhow::{Context, Result};
use mesh_crypto::Digest;
use mesh_object::{create, Author};
use mesh_site::app::{self, page_headers, prepare_page, script_headers, RESERVED};
use mesh_site::view::{headers, is_html, neutralise_html};
use mesh_site::{Address, Lookup, Site};
use std::fmt::Write as _;

/// A file for the viewer.
pub struct ViewFile {
    /// 200 found, 404 not part of the site, 503 not downloaded yet.
    pub status: u16,
    pub media_type: String,
    pub data: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

fn find(m: &Mobile, address: &str) -> Result<Site> {
    m.node.with_store(|s| mesh_site::find(s, address))
}

/// Same file and format as the CLI (`anonymous.key`, hex), so a home
/// folder works with both.
fn anonymous_secret(m: &Mobile) -> Result<[u8; 32]> {
    let path = m.home.join("anonymous.key");
    if !path.exists() {
        std::fs::write(&path, hex::encode(mesh_crypto::random_bytes::<32>()))?;
    }
    let raw = hex::decode(std::fs::read_to_string(&path)?.trim()).context("anonymous.key is not hex")?;
    raw.try_into().map_err(|_| anyhow::anyhow!("anonymous.key has the wrong length"))
}

impl Mobile {
    /// Sites held here, newest version of each, as a JSON array of
    /// `{"address","short","title","version","files","have","size","ownerless","app"}`.
    pub fn sites_json(&self) -> Result<String> {
        self.node.with_store(|s| {
            let mut out = String::from("[");
            for (i, site) in mesh_site::sites(s)?.iter().enumerate() {
                let (have, all) = site.downloaded(s)?;
                if i > 0 {
                    out.push(',');
                }
                let _ = write!(out, "{{\"address\":\"{}\",\"short\":\"{}\",\"title\":", site.address, site.address.short());
                push_json_str(&mut out, &site.manifest.title);
                let _ = write!(
                    out,
                    ",\"version\":{},\"files\":{all},\"have\":{have},\"size\":{},\"ownerless\":{},\"app\":{}}}",
                    site.manifest.version,
                    site.total_size(),
                    matches!(site.address, Address::Ownerless(_)),
                    site.manifest.app.is_some()
                );
            }
            out.push(']');
            Ok(out)
        })
    }

    /// Queue every file of a site for download. Returns the index path.
    pub fn site_want(&self, address: &str) -> Result<String> {
        let site = find(self, address)?;
        self.node.with_store(|s| mesh_site::want(s, &site))?;
        Ok(site.manifest.index.clone())
    }

    /// Files of the site present, and in total.
    pub fn site_progress(&self, address: &str) -> Result<(usize, usize)> {
        let site = find(self, address)?;
        self.node.with_store(|s| site.downloaded(s))
    }

    /// One file of the newest version of a site, ready to show. For an app,
    /// pages carry the host script (and its nonce policy), its scripts get
    /// the worker policy, and `.socialmesh/` serves the viewer's scripts.
    pub fn site_file(&self, address: &str, path: &str) -> Result<ViewFile> {
        let site = find(self, address)?;
        if let (Some(_), Some(reserved)) = (&site.manifest.app, path.strip_prefix(RESERVED)) {
            let body = match reserved {
                "host.js" => app::HOST_JS,
                "worker.js" => app::WORKER_JS,
                _ => return Ok(missing()),
            };
            let mut headers = if reserved == "worker.js" { script_headers() } else { headers("text/javascript") };
            headers.retain(|(k, _)| *k != "Content-Disposition");
            return Ok(ViewFile { status: 200, media_type: "text/javascript".into(), data: body.as_bytes().to_vec(), headers });
        }
        let lookup = if mesh_object::site::valid_path(path) {
            self.node.with_store(|s| mesh_site::read(s, &site, path))?
        } else {
            Lookup::Missing
        };
        Ok(match lookup {
            Lookup::Ready { data, media_type } => match &site.manifest.app {
                Some(script) if is_html(&media_type) => {
                    let nonce = app::nonce();
                    ViewFile { status: 200, headers: page_headers(&media_type, &nonce), data: prepare_page(data, path, script, &nonce), media_type }
                }
                Some(_) if media_type == "text/javascript" => ViewFile { status: 200, headers: script_headers(), media_type, data },
                _ => {
                    let data = if is_html(&media_type) { neutralise_html(data) } else { data };
                    ViewFile { status: 200, headers: headers(&media_type), media_type, data }
                }
            },
            Lookup::NotYet => page(503, "Still downloading", "This page hasn't arrived yet. It will appear when SocialMesh gets it from a nearby phone."),
            Lookup::Missing => missing(),
        })
    }

    /// `{"title","address","version"}` for an app's code.
    pub fn app_info(&self, address: &str) -> Result<String> {
        Ok(app::app_json(&find(self, address)?))
    }

    /// The identity IDs that count as this person for an app.
    fn mine(&self, site: &Site) -> Result<Vec<Digest>> {
        Ok(vec![app::app_key(&anonymous_secret(self)?, &site.address).public().id(), self.me.public().id()])
    }

    /// Entries to an app, as its code sees them (see `mesh_site::app::entries_json`).
    pub fn app_entries(&self, address: &str) -> Result<String> {
        let site = find(self, address)?;
        let mine = self.mine(&site)?;
        self.node.with_store(|s| app::entries_json(s, &site, &mine))
    }

    /// The short ID an anonymous entry to this app is signed with.
    pub fn app_anonymous_id(&self, address: &str) -> Result<String> {
        let site = find(self, address)?;
        Ok(app::app_key(&anonymous_secret(self)?, &site.address).public().id().short())
    }

    /// Publish an entry the person has agreed to: signed with their key for
    /// this app and sent by the private route (`anonymous`), or signed as
    /// them and announced directly. Returns its ID (hex).
    pub fn app_submit(&self, address: &str, data: &str, anonymous: bool) -> Result<String> {
        let site = find(self, address)?;
        let content = app::entry_content(&site, data)?;
        let obj = if anonymous {
            let mut c = content;
            c.created_ms -= c.created_ms % 60_000; // coarse, like anonymous posts
            let obj = create(c, Author::Signed(&app::app_key(&anonymous_secret(self)?, &site.address)))?;
            self.node.publish_anonymous(&obj)?;
            obj
        } else {
            let obj = create(content, Author::Signed(&self.me))?;
            self.node.publish(&obj)?;
            obj
        };
        Ok(obj.id().to_hex())
    }
}

fn page(status: u16, title: &str, text: &str) -> ViewFile {
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <body style=\"font:16px sans-serif;margin:2em 1em\"><h1 style=\"font-size:1.3em\">{title}</h1><p>{text}</p>"
    );
    let t = "text/html; charset=utf-8".to_string();
    ViewFile { status, headers: headers(&t), media_type: t, data: html.into_bytes() }
}

fn missing() -> ViewFile {
    page(404, "Not part of this site", "That file isn't in this site.")
}
