//! The phone's view of sites: listing, and files served under the viewer's rules.

use mesh_crypto::Keypair;
use mesh_object::{create, Author};
use mesh_site::NewFile;
use mesh_store::{BlobSource, Origin, Store};
use socialmesh::{Mobile, Options, SiteTarget};

fn seg(m: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, m];
    v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// A minimal JPEG whose EXIF block carries a location.
fn photo_with_gps() -> Vec<u8> {
    let mut f = vec![0xFF, 0xD8];
    f.extend(seg(0xE0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0"));
    f.extend(seg(0xE1, b"Exif\0\0GPS -35.3075 149.1244 Pixel 8"));
    f.extend(seg(0xDB, &[0u8; 65]));
    f.extend(seg(0xDA, &[1, 1, 0, 0, 63, 0]));
    f.extend([0x12, 0x34, 0xFF, 0xD9]);
    f
}

fn files(index: &str) -> Vec<(String, Vec<u8>)> {
    vec![
        ("index.html".into(), index.as_bytes().to_vec()),
        ("style.css".into(), b"h1{color:green}".to_vec()),
        ("img/rally.jpg".into(), photo_with_gps()),
    ]
}

#[test]
fn a_phone_publishes_updates_and_strips_photo_locations() {
    let dir = tempfile::tempdir().unwrap();
    let m = Mobile::start(dir.path(), Options { beacon: false, lan: false }).unwrap();
    assert_eq!(m.my_sites_json().unwrap(), "[]");

    let line = m.publish_site(SiteTarget::New, "Rally: Saturday!", "index.html", files("<h1>v1</h1>"), false).unwrap();
    assert!(line.contains("version 1: 3 files"), "{line}");
    let mine = m.my_sites_json().unwrap();
    assert!(mine.contains(r#""key":"rally-saturday""#) && mine.contains(r#""version":1"#), "{mine}");
    let address = mine.split(r#""address":""#).nth(1).unwrap().split('"').next().unwrap().to_string();

    // The photo went out without its location.
    let photo = m.site_file(&address, "img/rally.jpg").unwrap();
    assert_eq!(photo.status, 200);
    assert!(!photo.data.windows(3).any(|w| w == b"GPS"), "location survived");

    // An update re-sends only what changed.
    let line = m.publish_site(SiteTarget::Update("rally-saturday"), "Rally: Saturday!", "index.html", files("<h1>v2</h1>"), false).unwrap();
    assert!(line.contains("version 2: 3 files, 2 unchanged"), "{line}");
    assert_eq!(String::from_utf8(m.site_file(&address, "index.html").unwrap().data).unwrap(), "<h1>v2</h1>");

    // An SVG may hide its creator: refused until the user agrees, and no stray key is left.
    let mut with_svg = files("<h1>x</h1>");
    with_svg.push(("map.svg".into(), b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec()));
    let err = m.publish_site(SiteTarget::New, "Second", "index.html", with_svg.clone(), false).unwrap_err().to_string();
    assert!(err.starts_with("METADATA map.svg"), "{err}");
    assert!(!m.my_sites_json().unwrap().contains("second"), "no key for a failed publish");
    assert!(m.publish_site(SiteTarget::New, "Second", "index.html", with_svg, true).is_ok());

    // Anonymous: no key kept, can't be updated.
    let line = m.publish_site(SiteTarget::Ownerless, "Leaflet", "index.html", files("<h1>anon</h1>"), false).unwrap();
    assert!(line.contains("version 1"), "{line}");
    assert_eq!(m.my_sites_json().unwrap().matches(r#""key""#).count(), 2);

    // Bad input is refused.
    assert!(m.publish_site(SiteTarget::New, "Bad", "index.html", vec![("../x.html".into(), vec![])], false).is_err());
    assert!(m.publish_site(SiteTarget::Update("nope"), "T", "index.html", files(""), false).is_err());
}

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
