//! The phone's view of sites: listing, and files served under the viewer's rules.

use mesh_crypto::Keypair;
use mesh_object::{create, Author};
use mesh_site::NewFile;
use mesh_store::{BlobSource, Origin, Store};
use socialmesh::{Mobile, Options};

#[test]
fn a_site_is_listed_and_served_safely() {
    let dir = tempfile::tempdir().unwrap();
    let key = Keypair::generate();
    {
        let store = Store::open(dir.path()).unwrap();
        let files = vec![
            NewFile {
                path: "index.html".into(),
                media_type: "text/html".into(),
                data: br#"<META HTTP-EQUIV=refresh content="0;url=https://t.example"><h1>Water plan</h1>"#.to_vec(),
            },
            NewFile { path: "img/a.png".into(), media_type: "image/png".into(), data: vec![1, 2, 3] },
        ];
        let c = mesh_site::build(&store, files, "Water \"plan\"", "index.html", None, BlobSource::Own).unwrap();
        store.put(&create(c, Author::Signed(&key)).unwrap(), &Origin::Local).unwrap();
    }
    let m = Mobile::start(dir.path(), Options { beacon: false, lan: false }).unwrap();

    let json = m.sites_json().unwrap();
    let address = format!("site:{}", key.public().id().to_hex());
    assert!(json.contains(&address), "{json}");
    assert!(json.contains(r#""title":"Water \"plan\"""#) && json.contains(r#""have":2"#), "{json}");
    assert!(m.feed_json(10).unwrap() == "[]", "sites stay out of the post feed");

    assert_eq!(m.site_want(&address).unwrap(), "index.html");
    assert_eq!(m.site_progress(&address).unwrap(), (2, 2));

    let f = m.site_file(&address, "index.html").unwrap();
    assert_eq!(f.status, 200);
    let body = String::from_utf8(f.data).unwrap();
    assert!(body.contains("<h1>Water plan</h1>") && !body.to_ascii_lowercase().contains("http-equiv"), "{body}");
    assert!(f.headers.iter().any(|(k, v)| *k == "Content-Security-Policy" && v.starts_with("default-src 'none'")));

    for bad in ["../node.db", "/etc/passwd", "img/../index.html", "missing.html"] {
        assert_eq!(m.site_file(&address, bad).unwrap().status, 404, "{bad}");
    }
    assert!(m.site_file("site:00", "index.html").is_err(), "unknown site");
}
