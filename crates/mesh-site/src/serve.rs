//! A site served to an ordinary browser on 127.0.0.1 (SPEC.md §10.4, §11).
//!
//! Every URL starts with a random token, and the `Host` header must be this
//! server's own address, so neither another web page open in the browser
//! (including via DNS rebinding) nor a program that doesn't know the token
//! can read which sites this node holds or what is in them. Each request
//! reads the newest version from the store, so a refresh shows an update
//! as soon as it arrives.
//!
//! For an app, the reserved `.socialmesh/` paths serve the host and worker
//! scripts and a small API (`api/app`, `api/entries`, `api/submit`).
//! Submitting goes through an [`AppHost`], which asks the person and signs.

use crate::app::{self, page_headers, prepare_page, script_headers, RESERVED};
use crate::view::{headers, is_html, neutralise_html};
use crate::{latest, read, Address, Lookup, Site};
use anyhow::Result;
use mesh_crypto::Digest;
use mesh_object::site::valid_path;
use mesh_store::Store;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const MAX_REQUEST_HEAD: usize = 16 * 1024;
const MAX_REQUEST_BODY: usize = 64 * 1024;

/// What the viewer's owner does for an app: knows who "mine" is, and asks
/// before publishing an entry.
pub trait AppHost: Send + Sync {
    /// Identity IDs that count as this person for `site` (their app key and identity).
    fn mine(&self, site: &Site) -> Vec<Digest>;
    /// Ask the person; publish `data` as an entry if they agree. Errors (including
    /// "declined") go back to the app.
    fn submit(&self, store: &Store, site: &Site, data: &str, summary: &str) -> Result<Digest>;
}

pub struct Server {
    listener: TcpListener,
    ctx: Arc<Ctx>,
}

struct Ctx {
    home: PathBuf,
    address: Address,
    token: String,
    port: u16,
    app_host: Option<Arc<dyn AppHost>>,
}

impl Server {
    /// Bind 127.0.0.1:`port` (0 picks a free one) for the site at `address`
    /// in the store at `home`.
    pub fn bind(home: PathBuf, address: Address, port: u16) -> Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let port = listener.local_addr()?.port();
        let token = hex(&mesh_crypto::random_bytes::<16>());
        Ok(Server { listener, ctx: Arc::new(Ctx { home, address, token, port, app_host: None }) })
    }

    /// Run apps through `host`. Without one, apps show but can't submit.
    pub fn with_app_host(mut self, host: Arc<dyn AppHost>) -> Server {
        Arc::get_mut(&mut self.ctx).expect("not yet shared").app_host = Some(host);
        self
    }

    /// Where to point the browser.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/{}/", self.ctx.port, self.ctx.token)
    }

    /// Serve until the process ends.
    pub fn run(self) -> Result<()> {
        for conn in self.listener.incoming() {
            let Ok(stream) = conn else { continue };
            let ctx = self.ctx.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &ctx);
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
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

fn page(status: &'static str, title: &str, text: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <body style=\"font:16px sans-serif;max-width:36em;margin:3em auto;padding:0 1em\">\
         <h1 style=\"font-size:1.3em\">{title}</h1><p>{text}</p>"
    );
    Response { status, body: body.into_bytes(), headers: headers("text/html; charset=utf-8") }
}

fn json(status: &'static str, body: String) -> Response {
    let mut h = headers("application/json");
    h.retain(|(k, _)| *k != "Content-Disposition");
    Response { status, body: body.into_bytes(), headers: h }
}

fn json_error(status: &'static str, msg: &str) -> Response {
    let mut s = String::from("{\"error\":");
    app::json_str(&mut s, msg);
    s.push('}');
    json(status, s)
}

struct Request {
    method: String,
    target: String,
    host: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &TcpStream) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let (mut seen, mut line, mut first) = (0usize, String::new(), String::new());
    let (mut host, mut length) = (None, 0usize);
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        seen += line.len();
        if seen > MAX_REQUEST_HEAD {
            return Ok(None);
        }
        let l = line.trim_end_matches(['\r', '\n']);
        if l.is_empty() {
            break;
        }
        if first.is_empty() {
            first = l.to_string();
        } else if let Some((k, v)) = l.split_once(':') {
            let k = k.trim();
            if k.eq_ignore_ascii_case("host") {
                host = Some(v.trim().to_string());
            } else if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(usize::MAX);
            }
        }
    }
    if length > MAX_REQUEST_BODY {
        return Ok(None);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let mut parts = first.split(' ');
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    Ok(Some(Request { method, target, host, body }))
}

fn handle(stream: TcpStream, ctx: &Ctx) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let Some(req) = read_request(&stream)? else { return Ok(()) };
    let resp = respond(&req, ctx);
    write_response(stream, resp, req.method == "HEAD")
}

fn respond(req: &Request, ctx: &Ctx) -> Response {
    let ours = [format!("127.0.0.1:{}", ctx.port), format!("localhost:{}", ctx.port)];
    if !req.host.as_deref().is_some_and(|h| ours.iter().any(|o| o.eq_ignore_ascii_case(h))) {
        return page("403 Forbidden", "Not here", "This viewer only answers on its own address.");
    }
    let path = req.target.split(['?', '#']).next().unwrap_or("");
    let Some(rest) = path.strip_prefix('/').and_then(|p| p.strip_prefix(ctx.token.as_str())) else {
        return page("404 Not Found", "Not found", "Nothing here.");
    };
    let store = match Store::open(&ctx.home) {
        Ok(s) => s,
        Err(e) => return page("500 Internal Server Error", "Store unavailable", &escape(&e.to_string())),
    };
    let site = match latest(&store, &ctx.address) {
        Ok(Some(s)) => s,
        Ok(None) => return page("404 Not Found", "Site not here", "This node no longer holds that site."),
        Err(e) => return page("500 Internal Server Error", "Error", &escape(&e.to_string())),
    };
    let file = match rest {
        "" | "/" => {
            let mut r = page("302 Found", "Opening", "Opening the site…");
            r.headers.push(("Location", format!("/{}/{}", ctx.token, site.manifest.index)));
            return r;
        }
        p => p.strip_prefix('/').unwrap_or("\u{0}"),
    };
    if let Some(reserved) = file.strip_prefix(RESERVED) {
        return match site.manifest.app {
            Some(_) => viewer_route(req, reserved, &store, &site, ctx),
            None => page("404 Not Found", "Not found", "Nothing here."),
        };
    }
    if req.method != "GET" && req.method != "HEAD" {
        return page("405 Method Not Allowed", "Not allowed", "Sites are read-only.");
    }
    if !valid_path(file) {
        return page("404 Not Found", "Not part of this site", "That address isn't a file in this site.");
    }
    match read(&store, &site, file) {
        Ok(Lookup::Ready { data, media_type }) => match &site.manifest.app {
            Some(app_script) if is_html(&media_type) => {
                let nonce = app::nonce();
                Response {
                    status: "200 OK",
                    body: prepare_page(data, file, app_script, &nonce),
                    headers: page_headers(&media_type, &nonce),
                }
            }
            Some(_) if media_type == "text/javascript" => Response { status: "200 OK", body: data, headers: script_headers() },
            _ => {
                let body = if is_html(&media_type) { neutralise_html(data) } else { data };
                Response { status: "200 OK", body, headers: headers(&media_type) }
            }
        },
        Ok(Lookup::NotYet) => {
            let mut r = page(
                "503 Service Unavailable",
                "Still downloading",
                "This page hasn't arrived yet. Keep <code>mesh run</code> running near other nodes, then refresh.",
            );
            r.headers.push(("Retry-After", "10".into()));
            r
        }
        Ok(Lookup::Missing) => page("404 Not Found", "Not part of this site", "That file isn't in this site."),
        Err(e) => page("500 Internal Server Error", "Couldn't read the file", &escape(&e.to_string())),
    }
}

/// `.socialmesh/…` for an app.
fn viewer_route(req: &Request, path: &str, store: &Store, site: &Site, ctx: &Ctx) -> Response {
    let get = req.method == "GET" || req.method == "HEAD";
    match (path, get) {
        ("host.js", true) => {
            let mut h = headers("text/javascript");
            h.retain(|(k, _)| *k != "Content-Disposition");
            Response { status: "200 OK", body: app::HOST_JS.as_bytes().to_vec(), headers: h }
        }
        ("worker.js", true) => Response { status: "200 OK", body: app::WORKER_JS.as_bytes().to_vec(), headers: script_headers() },
        ("api/app", true) => json("200 OK", app::app_json(site)),
        ("api/entries", true) => {
            let mine = ctx.app_host.as_ref().map(|h| h.mine(site)).unwrap_or_default();
            match app::entries_json(store, site, &mine) {
                Ok(j) => json("200 OK", j),
                Err(e) => json_error("500 Internal Server Error", &e.to_string()),
            }
        }
        ("api/submit", false) if req.method == "POST" => {
            let Some(host) = &ctx.app_host else { return json_error("403 Forbidden", "this viewer can't publish entries") };
            let fields = std::str::from_utf8(&req.body).ok().and_then(parse_flat_json);
            let Some(fields) = fields else { return json_error("400 Bad Request", "bad request") };
            let data = fields.get("data").map(String::as_str).unwrap_or("");
            let summary = fields.get("summary").map(String::as_str).unwrap_or("");
            match host.submit(store, site, data, summary) {
                Ok(id) => json("200 OK", format!("{{\"id\":\"{}\"}}", id.to_hex())),
                Err(e) => json_error("200 OK", &e.to_string()),
            }
        }
        _ => page("404 Not Found", "Not found", "Nothing here."),
    }
}

/// A JSON object whose values are all strings (what the host script sends).
fn parse_flat_json(s: &str) -> Option<HashMap<String, String>> {
    fn ws(c: &mut std::iter::Peekable<std::str::Chars>) {
        while c.peek().is_some_and(|x| x.is_whitespace()) {
            c.next();
        }
    }
    fn string(c: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
        if c.next()? != '"' {
            return None;
        }
        let mut out = String::new();
        loop {
            match c.next()? {
                '"' => return Some(out),
                '\\' => match c.next()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let hex4 = |c: &mut std::iter::Peekable<std::str::Chars>| -> Option<u32> {
                            let h: String = (0..4).map(|_| c.next()).collect::<Option<String>>()?;
                            u32::from_str_radix(&h, 16).ok()
                        };
                        let hi = hex4(c)?;
                        let cp = if (0xD800..0xDC00).contains(&hi) {
                            if c.next()? != '\\' || c.next()? != 'u' {
                                return None;
                            }
                            let lo = hex4(c)?;
                            0x10000 + ((hi - 0xD800) << 10) + (lo.checked_sub(0xDC00)?)
                        } else {
                            hi
                        };
                        out.push(char::from_u32(cp)?);
                    }
                    _ => return None,
                },
                ch => out.push(ch),
            }
        }
    }
    let mut c = s.chars().peekable();
    let mut out = HashMap::new();
    ws(&mut c);
    if c.next()? != '{' {
        return None;
    }
    loop {
        ws(&mut c);
        if c.peek() == Some(&'}') {
            c.next();
            break;
        }
        let k = string(&mut c)?;
        ws(&mut c);
        if c.next()? != ':' {
            return None;
        }
        ws(&mut c);
        let v = string(&mut c)?;
        out.insert(k, v);
        ws(&mut c);
        match c.next()? {
            ',' => continue,
            '}' => break,
            _ => return None,
        }
    }
    ws(&mut c);
    c.next().is_none().then_some(out)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn write_response(mut stream: TcpStream, r: Response, head_only: bool) -> Result<()> {
    let mut out = format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
    for (k, v) in r.headers {
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
    use crate::{build, build_app, NewFile};
    use mesh_crypto::Keypair;
    use mesh_object::{create, Author};
    use mesh_store::{BlobSource, Origin};

    fn request(port: u16, method: &str, path: &str, host: &str, body: &str) -> (String, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        let (head, body) = buf.split_once("\r\n\r\n").unwrap();
        (head.to_string(), body.to_string())
    }

    fn get(port: u16, path: &str, host: &str) -> (String, String) {
        request(port, "GET", path, host, "")
    }

    fn start(server: Server) -> (u16, String) {
        let (port, token) = (server.ctx.port, server.ctx.token.clone());
        std::thread::spawn(move || server.run());
        (port, token)
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

        let (port, token) = start(Server::bind(dir.path().to_path_buf(), Address::Keyed(key.public().id()), 0).unwrap());
        let host = format!("127.0.0.1:{port}");

        let (head, _) = get(port, &format!("/{token}/"), &host);
        assert!(head.starts_with("HTTP/1.1 302") && head.contains(&format!("Location: /{token}/index.html")), "{head}");

        let (head, body) = get(port, &format!("/{token}/index.html"), &host);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(head.contains("Content-Security-Policy: default-src 'none'"));
        assert!(body.contains("<h1>Hello</h1>") && !body.to_ascii_lowercase().contains("http-equiv"), "{body}");

        let (head, _) = get(port, &format!("/{token}/tool.js"), &host);
        assert!(head.contains("Content-Disposition: attachment"), "scripts are downloads: {head}");
        assert!(get(port, &format!("/{token}/.socialmesh/host.js"), &host).0.starts_with("HTTP/1.1 404"), "not an app");

        assert!(get(port, "/index.html", &host).0.starts_with("HTTP/1.1 404"), "no token");
        assert!(get(port, &format!("/{token}/index.html"), "evil.example").0.starts_with("HTTP/1.1 403"), "rebinding");
        assert!(get(port, &format!("/{token}/../node.db"), &host).0.starts_with("HTTP/1.1 404"));
        assert!(get(port, &format!("/{token}/missing.html"), &host).0.starts_with("HTTP/1.1 404"));
    }

    struct Yes(std::sync::Mutex<Vec<String>>);
    impl AppHost for Yes {
        fn mine(&self, _: &Site) -> Vec<Digest> {
            vec![]
        }
        fn submit(&self, store: &Store, site: &Site, data: &str, summary: &str) -> Result<Digest> {
            self.0.lock().unwrap().push(format!("{summary}|{data}"));
            let obj = create(app::entry_content(site, data)?, Author::Signed(&Keypair::generate()))?;
            store.put(&obj, &Origin::Local)?;
            Ok(obj.id())
        }
    }

    #[test]
    fn an_app_gets_its_host_policies_and_api() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let key = Keypair::generate();
        let files = vec![
            NewFile {
                path: "index.html".into(),
                media_type: "text/html".into(),
                data: b"<head><title>Poll</title></head><div data-socialmesh-app></div><script>evil()</script>".to_vec(),
            },
            NewFile { path: "poll.js".into(), media_type: "text/javascript".into(), data: b"socialmesh.render('hi')".to_vec() },
        ];
        let c = build_app(&store, files, "Poll", "index.html", Some("poll.js"), None, BlobSource::Own).unwrap();
        store.put(&create(c, Author::Signed(&key)).unwrap(), &Origin::Local).unwrap();
        drop(store);

        let asked = Arc::new(Yes(Default::default()));
        let server = Server::bind(dir.path().to_path_buf(), Address::Keyed(key.public().id()), 0).unwrap().with_app_host(asked.clone());
        let (port, token) = start(server);
        let host = format!("127.0.0.1:{port}");

        let (head, body) = get(port, &format!("/{token}/index.html"), &host);
        let nonce = body.split("nonce=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        assert!(head.contains(&format!("script-src 'nonce-{nonce}';")), "{head}");
        assert!(body.contains(r#"<title>"#) && body.find("host.js").unwrap() < body.find("<title>").unwrap());

        let (head, _) = get(port, &format!("/{token}/poll.js"), &host);
        assert!(head.contains("Content-Security-Policy: default-src 'none'; script-src 'self'") && !head.contains("attachment"), "{head}");
        let (head, body) = get(port, &format!("/{token}/.socialmesh/worker.js"), &host);
        assert!(head.contains("script-src 'self'") && body.contains("importScripts"), "{head}");
        assert!(get(port, &format!("/{token}/.socialmesh/host.js"), &host).1.contains("data-socialmesh-app"));
        assert!(get(port, &format!("/{token}/.socialmesh/api/app"), &host).1.contains(r#""title":"Poll""#));
        assert_eq!(get(port, &format!("/{token}/.socialmesh/api/entries"), &host).1, "[]");

        let body = r#"{"data":"{\"vote\":\"yes é\"}","summary":"Vote yes"}"#;
        let (_, reply) = request(port, "POST", &format!("/{token}/.socialmesh/api/submit"), &host, body);
        assert!(reply.starts_with("{\"id\":\""), "{reply}");
        assert_eq!(asked.0.lock().unwrap()[0], "Vote yes|{\"vote\":\"yes é\"}");
        let entries = get(port, &format!("/{token}/.socialmesh/api/entries"), &host).1;
        assert!(entries.contains(r#""data":"{\"vote\":\"yes é\"}""#), "{entries}");
        assert!(request(port, "POST", &format!("/{token}/index.html"), &host, "").0.starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn flat_json_parsing() {
        let m = parse_flat_json(r#" { "a" : "x\"y", "b":"😀\n" } "#).unwrap();
        assert_eq!(m["a"], "x\"y");
        assert_eq!(m["b"], "😀\n");
        for bad in ["", "{", r#"{"a":1}"#, r#"{"a":"x"} trailing"#, r#"{"a":"\q"}"#] {
            assert!(parse_flat_json(bad).is_none(), "{bad}");
        }
    }
}
