//! Viewing rules (SPEC.md §10.4): what a viewer sends with each file, and
//! how HTML is made unable to reach outside the site.
//!
//! The browser enforces most of it through the response headers: a content
//! security policy that allows nothing from outside the site and no scripts,
//! plus a sandbox with no scripts or forms. Two things a policy can't stop
//! are handled in the HTML itself: `<meta http-equiv="refresh">` redirects,
//! and connection hints (`preconnect`, `dns-prefetch`, `prerender`) that make
//! a browser contact a server, and so reveal the reader's address, without
//! loading anything.

/// The content security policy for every response.
pub const CSP: &str = "default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'self' 'unsafe-inline'; \
     font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; sandbox allow-same-origin";

/// Headers for a file of `media_type` (and for the viewer's own pages).
pub fn headers(media_type: &str) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("Content-Type", media_type.to_string()),
        ("Content-Security-Policy", CSP.to_string()),
        ("X-Content-Type-Options", "nosniff".into()),
        ("Referrer-Policy", "no-referrer".into()),
        ("X-DNS-Prefetch-Control", "off".into()),
        ("Cross-Origin-Resource-Policy", "same-origin".into()),
        ("Cross-Origin-Opener-Policy", "same-origin".into()),
        ("Cache-Control", "no-store".into()),
    ];
    if !renderable(media_type) {
        // Scripts, PDFs, archives and unknown types are offered as downloads,
        // never opened in the page.
        h.push(("Content-Disposition", "attachment".into()));
    }
    h
}

/// Types shown in the browser. Everything else is a download.
pub fn renderable(media_type: &str) -> bool {
    let t = media_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    matches!(t.as_str(), "text/html" | "text/css" | "text/plain")
        || ["image/", "audio/", "video/", "font/"].iter().any(|p| t.starts_with(p))
}

pub fn is_html(media_type: &str) -> bool {
    let t = media_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    t == "text/html" || t == "application/xhtml+xml"
}

/// Words that, as attribute names or link relations, would make a browser
/// navigate or connect somewhere, each with a same-length harmless stand-in.
/// Browsers match them case-insensitively in ASCII, with no escapes, so
/// replacing every occurrence anywhere in the bytes leaves no way to write
/// one. (Ordinary text that mentions them changes too; that is the price.)
///
/// Frames and embedded objects are renamed to unknown elements: a browser
/// preconnects (a real DNS lookup) to an `<iframe>`'s address before its
/// policy refuses the frame, and a tag name must follow `<` directly, so
/// these spellings are the only ones.
const NEUTRALISE: [(&[u8], &[u8]); 10] = [
    (b"http-equiv", b"data-equiv"),
    (b"preconnect", b"xreconnect"),
    (b"dns-prefetch", b"dns-xrefetch"),
    (b"prerender", b"xrerender"),
    (b"<iframe", b"<xframe"),
    (b"<frame", b"<xrame"),
    (b"<object", b"<xbject"),
    (b"<embed", b"<xmbed"),
    (b"<portal", b"<xortal"),
    (b"<fencedframe", b"<xencedframe"),
];

/// Make site HTML unable to redirect itself or contact anything.
pub fn neutralise_html(mut html: Vec<u8>) -> Vec<u8> {
    for (word, stand_in) in NEUTRALISE {
        debug_assert_eq!(word.len(), stand_in.len());
        let mut i = 0;
        while i + word.len() <= html.len() {
            if html[i..i + word.len()].eq_ignore_ascii_case(word) {
                html[i..i + word.len()].copy_from_slice(stand_in);
                i += word.len();
            } else {
                i += 1;
            }
        }
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        String::from_utf8(neutralise_html(s.as_bytes().to_vec())).unwrap()
    }

    #[test]
    fn redirects_and_connection_hints_are_neutralised_however_written() {
        let attacks = [
            r#"<meta http-equiv="refresh" content="0;url=https://tracker.example/">"#,
            r#"<META HTTP-EQUIV=Refresh CONTENT="0;url=https://t.example">"#,
            r#"<meta content="a>b" http-equiv=refresh>"#,
            r#"<meta a=x" b="q>z" Http-Equiv=refresh content=0;url=//t.example>"#,
            r#"<!-- <meta x=" --><meta http-equiv=refresh content=0;url=//t.example>"#,
            r#"<meta/http-equiv/=refresh content=0;url=//t.example>"#,
            r#"<link rel="preconnect" href="https://t.example">"#,
            r#"<link rel="DNS-Prefetch" href="//t.example">"#,
            r#"<link rel=prerender href=https://t.example>"#,
            r#"<link rel="stylesheet preconnect" href="https://t.example">"#,
            r#"<iframe src="https://t.example/frame"></iframe>"#,
            r#"<IFRAME SRC=//t.example>"#,
            r#"<frameset><frame src="https://t.example/f"></frameset>"#,
            r#"<object data="https://t.example/o"></object><EMBED src=//t.example>"#,
            r#"<portal src="https://t.example"></portal><fencedframe></fencedframe>"#,
        ];
        for a in attacks {
            let out = clean(a).to_ascii_lowercase();
            for (word, _) in NEUTRALISE {
                assert!(!out.contains(std::str::from_utf8(word).unwrap()), "{a} -> {out}");
            }
        }
    }

    #[test]
    fn ordinary_html_is_left_alone() {
        let page = "<!doctype html><title>Hi</title><link rel=stylesheet href=style.css><p>Text <a href=b.html>next</a></p>";
        assert_eq!(clean(page), page);
    }

    #[test]
    fn only_display_types_render() {
        for t in ["text/html", "text/css", "image/webp", "image/svg+xml", "font/woff2", "video/mp4", "text/plain; charset=utf-8"] {
            assert!(renderable(t), "{t}");
        }
        for t in ["text/javascript", "application/pdf", "application/zip", "application/octet-stream", "application/xhtml+xml"] {
            assert!(!renderable(t), "{t}");
            assert!(headers(t).iter().any(|(k, v)| *k == "Content-Disposition" && v == "attachment"));
        }
        let h = headers("text/html");
        let csp = &h.iter().find(|(k, _)| *k == "Content-Security-Policy").unwrap().1;
        assert!(csp.starts_with("default-src 'none'") && !csp.contains("script-src") && csp.contains("sandbox"));
    }
}
