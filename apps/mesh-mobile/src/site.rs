//! Sites for the phone app (SPEC.md §10): list them, queue their files, and
//! hand the app's viewer one file at a time, under the same rules as the
//! desktop viewer (`mesh_site::view`).

use crate::{push_json_str, Mobile};
use anyhow::Result;
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

impl Mobile {
    /// Sites held here, newest version of each, as a JSON array of
    /// `{"address","short","title","version","files","have","size"}`.
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
                    ",\"version\":{},\"files\":{all},\"have\":{have},\"size\":{},\"ownerless\":{}}}",
                    site.manifest.version,
                    site.total_size(),
                    matches!(site.address, Address::Ownerless(_))
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

    /// One file of the newest version of a site, ready to show.
    pub fn site_file(&self, address: &str, path: &str) -> Result<ViewFile> {
        let site = find(self, address)?;
        let lookup = if mesh_object::site::valid_path(path) {
            self.node.with_store(|s| mesh_site::read(s, &site, path))?
        } else {
            Lookup::Missing
        };
        let page = |status: u16, title: &str, text: &str| {
            let html = format!(
                "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
                 <body style=\"font:16px sans-serif;margin:2em 1em\"><h1 style=\"font-size:1.3em\">{title}</h1><p>{text}</p>"
            );
            let t = "text/html; charset=utf-8".to_string();
            ViewFile { status, headers: headers(&t), media_type: t, data: html.into_bytes() }
        };
        Ok(match lookup {
            Lookup::Ready { data, media_type } => {
                let data = if is_html(&media_type) { neutralise_html(data) } else { data };
                ViewFile { status: 200, headers: headers(&media_type), media_type, data }
            }
            Lookup::NotYet => page(503, "Still downloading", "This page hasn't arrived yet. It will appear when SocialMesh gets it from a nearby phone."),
            Lookup::Missing => page(404, "Not part of this site", "That file isn't in this site."),
        })
    }
}
