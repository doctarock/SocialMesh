//! A site served to an ordinary browser on 127.0.0.1 (SPEC.md §10.4).
//!
//! Every URL starts with a random token, and the `Host` header must be this
//! server's own address, so neither another web page open in the browser
//! (including via DNS rebinding) nor a program that doesn't know the token
//! can read which sites this node holds or what is in them. Each request
//! reads the newest version from the store, so a refresh shows an update
//! as soon as it arrives.

use crate::view::{headers, is_html, neutralise_html};
use crate::{latest, read, Address, Lookup};
use anyhow::Result;
use mesh_object::site::valid_path;
use mesh_store::Store;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const MAX_REQUEST_HEAD: usize = 16 * 1024;

pub struct Server {
    listener: TcpListener,
    home: Arc<PathBuf>,
    address: Address,
    token: Arc<String>,
    port: u16,
}

impl Server {
    /// Bind 127.0.0.1:`port` (0 picks a free one) for the site at `address`
    /// in the store at `home`.
    pub fn bind(home: PathBuf, address: Address, port: u16) -> Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        let token = hex(&mesh_crypto::random_bytes::<16>());
        Ok(Server { listener, home: Arc::new(home), address, token: Arc::new(token), port })
    }

    /// Where to point the browser.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/", self.port, self.token)
    }

    /// Serve until the process ends.
    pub fn run(self) -> Result<()> {
        for conn in self.listener.incoming() {
            let Ok(stream) = conn else { continue };
            let (home, token, address, port) = (self.home.clone(), self.token.clone(), self.address, self.port);
            std::thread::spawn(move || {
                let _ = handle(stream, &home, &token, &address, port);
            });
        }
        Ok(())
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Response {
    status: &'static str,
    media_type: String,
    body: Vec<u8>,
    extra: Vec<(&'static str, String)>,
}

fn page(status: &'static str, title: &str, text: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font:16px sans-serif;max-width:36em;margin:3em auto;padding:0 1em\">\
         <h1 style=\"font-size:1.3em\">{title}</h1><p>{text}</p>"
    );
    Response { status, media_type: "text/html; charset=utf-8".into(), body: body.into_bytes(), extra: vec![] }
}

fn handle(stream: TcpStream, home: &std::path::Path, token: &str, address: &Address, port: u16) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = Vec::new();
    let mut line = String::new();
    let mut first = String::new();
    let mut host = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        head.push(line.len());
        if head.iter().sum::<usize>() > MAX_REQUEST_HEAD {
            return Ok(());
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        if first.is_empty() {
            first = l.to_string();
        } else if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("host") {
                host = Some(v.trim().to_string());
            }
        }
    }
    let mut parts = first.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let head_only = method == "HEAD";
    let resp = respond(method, target, host.as_deref(), home, token, address, port);
    write_response(stream, resp, head_only)
}

fn respond(method: &str, target: &str, host: Option<&str>, home: &std::path::Path, token: &str, address: &Address, port: u16) -> Response {
    let ours = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if !host.is_some_and(|h| ours.iter().any(|o| o.eq_ignore_ascii_case(h))) {
        return page("403 Forbidden", "Not here", "This viewer only answers on its own address.");
    }
    if method != "GET" && method != "HEAD" {
        return page("405 Method Not Allowed", "Not allowed", "Sites are read-only.");
    }
    let path = target.split(['?', '#']).next().unwrap_or("");
    let Some(rest) = path.strip_prefix('/').and_then(|p| p.strip_prefix(token)) else {
        return page("404 Not Found", "Not found", "Nothing here.");
    };
    let store = match Store::open(home) {
        Ok(s) => s,
        Err(e) => return page("500 Internal Server Error", "Store unavailable", &escape(&e.to_string())),
    };
    let site = match latest(&store, address) {
        Ok(Some(s)) => s,
        Ok(None) => return page("404 Not Found", "Site not here", "This node no longer holds that site."),
        Err(e) => return page("500 Internal Server Error", "Error", &escape(&e.to_string())),
    };
    let file = match rest {
        "" | "/" => {
            let mut r = page("302 Found", "Opening", "Opening the site…");
            r.extra.push(("Location", format!("/{token}/{}", site.manifest.index)));
            return r;
        }
        p => p.strip_prefix('/').unwrap_or("\u{0}"),
    };
    if !valid_path(file) {
        return page("404 Not Found", "Not part of this site", "That address isn't a file in this site.");
    }
    match read(&store, &site, file) {
        Ok(Lookup::Ready { data, media_type }) => {
            let body = if is_html(&media_type) { neutralise_html(data) } else { data };
            Response { status: "200 OK", media_type, body, extra: vec![] }
        }
        Ok(Lookup::NotYet) => {
            let mut r = page(
                "503 Service Unavailable",
                "Still downloading",
                "This page hasn't arrived yet. Keep <code>mesh run</code> running near other nodes, then refresh.",
            );
            r.extra.push(("Retry-After", "10".into()));
            r
        }
        Ok(Lookup::Missing) => page("404 Not Found", "Not part of this site", "That file isn't in this site."),
        Err(e) => page("500 Internal Server Error", "Couldn't read the file", &escape(&e.to_string())),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn write_response(mut stream: TcpStream, r: Response, head_only: bool) -> Result<()> {
    let mut out = format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
    for (k, v) in headers(&r.media_type).into_iter().chain(r.extra) {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes())?;
    if !head_only {
        stream.write_all(&r.body)?;
    }
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build, NewFile};
    use mesh_crypto::Keypair;
    use mesh_object::{create, Author};
    use mesh_store::{BlobSource, Origin};
    use std::io::Read;

    fn get(port: u16, path: &str, host: &str) -> (String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        let (head, body) = buf.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    #[test]
    fn serves_the_site_only_with_the_token_and_host_and_strips_redirects() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = Keypair::generate();
        let files = vec![
            NewFile {
                path: "index.html".into(),
                media_type: "text/html".into(),
                data: br#"<meta http-equiv="refresh" content="0;url=https://t.example"><h1>Hello</h1>"#.to_vec(),
            },
            NewFile { path: "tool.js".into(), media_type: "text/javascript".into(), data: b"alert(1)".to_vec() },
        ];
        let c = build(&store, files, "Hi", "index.html", None, BlobSource::Own).unwrap();
        store.put(&create(c, Author::Signed(&key)).unwrap(), &Origin::Local).unwrap();
        drop(store);

        let server = Server::bind(dir.path().to_path_buf(), Address::Keyed(key.public().id()), 0).unwrap();
        let port = server.port;
        let token = server.token.to_string();
        std::thread::spawn(move || server.run());
        let host = format!("127.0.0.1:{port}");

        let (head, _) = get(port, &format!("/{token}/"), &host);
        assert!(head.starts_with("HTTP/1.1 302") && head.contains(&format!("Location: /{token}/index.html")), "{head}");

        let (head, body) = get(port, &format!("/{token}/index.html"), &host);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Security-Policy: default-src 'none'"));
        assert!(body.contains("<h1>Hello</h1>") && !body.to_ascii_lowercase().contains("http-equiv"), "{body}");

        let (head, _) = get(port, &format!("/{token}/tool.js"), &host);
        assert!(head.contains("Content-Disposition: attachment"), "scripts are downloads: {head}");

        assert!(get(port, "/index.html", &host).0.starts_with("HTTP/1.1 404"), "no token");
        assert!(get(port, &format!("/{token}/index.html"), "evil.example").0.starts_with("HTTP/1.1 403"), "rebinding");
        assert!(get(port, &format!("/{token}/../node.db"), &host).0.starts_with("HTTP/1.1 404"));
        assert!(get(port, &format!("/{token}/missing.html"), &host).0.starts_with("HTTP/1.1 404"));
    }
}
