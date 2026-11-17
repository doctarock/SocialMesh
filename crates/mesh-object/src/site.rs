//! Site manifests (SPEC.md §10.2): the body of a `site` object, mapping
//! paths to the object's attachments.

use crate::Object;

pub const KIND_SITE: &str = "site";
pub const SITE_BODY_TYPE: &str = "application/vnd.socialmesh.site";
const MAGIC_LINE: &str = "socialmesh-site 1";
pub const MAX_TITLE_CHARS: usize = 200;
pub const MAX_PATH_LEN: usize = 255;

/// What people send an app (SPEC.md §11.4).
pub const KIND_APP_ENTRY: &str = "app.entry";
pub const APP_ENTRY_BODY_TYPE: &str = "application/json";
pub const MAX_ENTRY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteFile {
    pub path: String,
    /// Index into the object's attachments.
    pub attachment: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiteManifest {
    pub version: u64,
    pub title: String,
    pub index: String,
    /// The app's script (SPEC.md §11.1); `None` for a plain site.
    pub app: Option<String>,
    /// Sorted by path.
    pub files: Vec<SiteFile>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid site: {0}")]
pub struct SiteError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, SiteError> {
    Err(SiteError(msg.into()))
}

/// Whether `path` is a valid site path (SPEC.md §10.2).
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_LEN
        && path.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
        })
}

fn valid_title(t: &str) -> bool {
    let n = t.chars().count();
    (1..=MAX_TITLE_CHARS).contains(&n) && !t.chars().any(char::is_control) && t.trim_end() == t
}

impl SiteManifest {
    /// Check everything that doesn't need the object: title, paths, order,
    /// index. `attachments` is how many the object carries.
    pub fn check(&self, attachments: usize) -> Result<(), SiteError> {
        if !valid_title(&self.title) {
            return err("title must be 1–200 characters with no control characters or trailing space");
        }
        if self.files.len() != attachments {
            return err(format!("{} files listed for {attachments} attachments", self.files.len()));
        }
        let mut seen = vec![false; attachments];
        for (i, f) in self.files.iter().enumerate() {
            if !valid_path(&f.path) {
                return err(format!("bad path {:?}", f.path));
            }
            if i > 0 && self.files[i - 1].path.as_bytes() >= f.path.as_bytes() {
                return err(format!("paths out of order or repeated at {:?}", f.path));
            }
            match seen.get_mut(f.attachment) {
                Some(s) if !*s => *s = true,
                _ => return err(format!("attachment {} listed twice or missing", f.attachment)),
            }
        }
        if !self.files.iter().any(|f| f.path == self.index) {
            return err(format!("index {:?} is not one of the files", self.index));
        }
        if let Some(app) = &self.app {
            if !app.ends_with(".js") || !self.files.iter().any(|f| &f.path == app) {
                return err(format!("app script {app:?} must be one of the files and end in .js"));
            }
        }
        Ok(())
    }

    /// The manifest text. Files are sorted first; call [`check`](Self::check)
    /// (or build the object, which does) before relying on it.
    pub fn encode(&self) -> String {
        let mut files = self.files.clone();
        files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        let mut s = format!("{MAGIC_LINE}\nversion {}\ntitle {}\nindex {}\n", self.version, self.title, self.index);
        if let Some(app) = &self.app {
            s.push_str(&format!("app {app}\n"));
        }
        for f in &files {
            s.push_str(&format!("file {} {}\n", f.attachment, f.path));
        }
        s
    }

    /// Parse and fully check a manifest body for an object with `attachments`
    /// attachments. Only the exact encoding is accepted.
    pub fn parse(body: &str, attachments: usize) -> Result<SiteManifest, SiteError> {
        let Some(rest) = body.strip_suffix('\n') else { return err("must end with a newline") };
        let mut lines = rest.split('\n').peekable();
        if lines.next() != Some(MAGIC_LINE) {
            return err("not a socialmesh-site 1 manifest");
        }
        let mut field = |name: &str| -> Result<String, SiteError> {
            match lines.next().and_then(|l| l.strip_prefix(name)).and_then(|l| l.strip_prefix(' ')) {
                Some(v) => Ok(v.to_string()),
                None => err(format!("expected `{name}` line")),
            }
        };
        let version_text = field("version")?;
        let version = match version_text.parse::<u64>() {
            Ok(v) if version_text == v.to_string() => v,
            _ => return err("bad version"),
        };
        let title = field("title")?;
        let index = field("index")?;
        let app = match lines.peek().and_then(|l| l.strip_prefix("app ")) {
            Some(a) => {
                let a = a.to_string();
                lines.next();
                Some(a)
            }
            None => None,
        };
        let mut files = Vec::new();
        for line in lines {
            let Some(rest) = line.strip_prefix("file ") else { return err(format!("unexpected line {line:?}")) };
            let Some((n, path)) = rest.split_once(' ') else { return err(format!("bad file line {line:?}")) };
            let attachment = match n.parse::<usize>() {
                Ok(v) if n == v.to_string() => v,
                _ => return err(format!("bad attachment index in {line:?}")),
            };
            files.push(SiteFile { path: path.to_string(), attachment });
        }
        let m = SiteManifest { version, title, index, app, files };
        m.check(attachments)?;
        if m.encode() != body {
            return err("not in canonical form");
        }
        Ok(m)
    }

    /// The manifest of `obj` if it is a valid site.
    pub fn of(obj: &Object) -> Option<Result<SiteManifest, SiteError>> {
        (obj.kind == KIND_SITE).then(|| {
            if obj.body_type != SITE_BODY_TYPE {
                return err("wrong body type");
            }
            if obj.parent.is_some() {
                return err("a site has no parent");
            }
            SiteManifest::parse(&obj.body, obj.attachments.len())
        })
    }

    pub fn file(&self, path: &str) -> Option<&SiteFile> {
        self.files.binary_search_by(|f| f.path.as_bytes().cmp(path.as_bytes())).ok().map(|i| &self.files[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SiteManifest {
        SiteManifest {
            version: 3,
            title: "Vote yes: why".into(),
            index: "index.html".into(),
            app: None,
            files: vec![
                SiteFile { path: "style.css".into(), attachment: 1 },
                SiteFile { path: "img/map.webp".into(), attachment: 2 },
                SiteFile { path: "index.html".into(), attachment: 0 },
            ],
        }
    }

    #[test]
    fn roundtrip_is_canonical_and_sorted() {
        let text = sample().encode();
        assert_eq!(
            text,
            "socialmesh-site 1\nversion 3\ntitle Vote yes: why\nindex index.html\n\
             file 2 img/map.webp\nfile 0 index.html\nfile 1 style.css\n"
        );
        let m = SiteManifest::parse(&text, 3).unwrap();
        assert_eq!(m.encode(), text);
        assert_eq!(m.file("style.css").unwrap().attachment, 1);
        assert!(m.file("nope.css").is_none());
    }

    #[test]
    fn an_app_line_names_a_script_among_the_files() {
        let mut m = sample();
        m.files.push(SiteFile { path: "poll.js".into(), attachment: 3 });
        m.app = Some("poll.js".into());
        let text = m.encode();
        assert!(text.contains("index index.html\napp poll.js\nfile "), "{text}");
        assert_eq!(SiteManifest::parse(&text, 4).unwrap().app.as_deref(), Some("poll.js"));
        for bad in ["style.css", "missing.js"] {
            m.app = Some(bad.into());
            assert!(SiteManifest::parse(&m.encode(), 4).is_err(), "{bad}");
        }
    }

    #[test]
    fn paths_are_strict() {
        for good in ["index.html", "a/b/c.png", "x~y_z-1.css", "..a"] {
            assert!(valid_path(good), "{good}");
        }
        for bad in ["", "/abs", "a//b", "a/./b", "../up", "a/..", "back\\slash", "spa ce", "q?x", "pct%20", "a/", "é.html"] {
            assert!(!valid_path(bad), "{bad}");
        }
        assert!(!valid_path(&"a".repeat(256)));
    }

    #[test]
    fn rejects_anything_off() {
        let ok = sample().encode();
        assert!(SiteManifest::parse(&ok, 4).is_err(), "attachment count must match");
        let cases = [
            ok.replace("socialmesh-site 1", "socialmesh-site 2"),
            ok.replace("version 3", "version 03"),
            ok.replace("version 3", "version -3"),
            ok.replace("index index.html", "index missing.html"),
            ok.replace("file 1 style.css", "file 0 style.css"),
            ok.replace("file 1 style.css", "file 1 ../style.css"),
            ok.replace("title Vote yes: why", "title "),
            ok.replace("title Vote yes: why", "title bell\u{7}"),
            ok.trim_end().to_string(),
            ok.replace("file 0 index.html\n", "file 0 index.html\nextra line\n"),
            // Out of order.
            "socialmesh-site 1\nversion 1\ntitle t\nindex b\nfile 0 b\nfile 1 a\n".into(),
        ];
        for c in cases {
            assert!(SiteManifest::parse(&c, 3).is_err(), "accepted {c:?}");
        }
    }
}
