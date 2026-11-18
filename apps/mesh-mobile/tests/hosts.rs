//! Hosts entered by hand: saved, pinned to the identity that answered, and
//! kept in sync; wrong identities refused.

use mesh_crypto::Keypair;
use mesh_node::{Config, Node};
use mesh_store::Store;
use mesh_transport_tcp::TcpTransport;
use socialmesh::{Mobile, Options};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_host_typed_in_is_pinned_and_synced_and_impostors_are_refused() {
    // A host: a computer running a node on this machine.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let host_dir = tempfile::tempdir().unwrap();
    let host_key = Keypair::generate();
    let host_id = host_key.public().id();
    let host = Node::new(
        host_key,
        Store::open(host_dir.path()).unwrap(),
        Arc::new(TcpTransport),
        Config { listen: Some("127.0.0.1:0".into()), ..Config::default() },
    );
    let running = rt.block_on(host.start()).unwrap();
    let addr = running.local_addr.clone().unwrap();
    let author = Keypair::generate();
    let post = mesh_object::create(mesh_object::Content::post("from the host"), mesh_object::Author::Signed(&author)).unwrap();
    host.publish(&post).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let m = Mobile::start(dir.path(), Options { beacon: false, lan: false }).unwrap();
    assert_eq!(m.hosts_json().unwrap(), r#"{"saved":[],"found":[]}"#);

    // An invitation naming someone else is refused and not saved.
    let impostor = format!("mesh1:{}@{addr}", Keypair::generate().public().id().to_hex());
    let line = m.add_host(&impostor).unwrap();
    assert!(line.starts_with("Refused"), "{line}");
    assert!(!m.hosts_json().unwrap().contains(&addr));

    // The real address: connected, pinned, and synced.
    let line = m.add_host(&format!("tcp://{addr}")).unwrap();
    assert!(line.starts_with(&format!("Connected to {}", host_id.short())), "{line}");
    let hosts = m.hosts_json().unwrap();
    assert!(hosts.contains(&format!(r#""addr":"{addr}","id":"{}""#, host_id.short())), "{hosts}");
    wait_for("the host's post", || m.feed_json(10).unwrap().contains(&post.id().to_hex()));
    wait_for("the link to show", || m.hosts_json().unwrap().contains(r#""linked":true"#));

    // Nobody there: saved anyway, to be retried.
    let line = m.add_host("127.0.0.1:1").unwrap();
    assert!(line.contains("will keep trying"), "{line}");
    assert!(m.add_host("not an address").is_err());

    assert!(m.remove_host("127.0.0.1:1").unwrap());
    assert!(!m.remove_host("127.0.0.1:1").unwrap());
    m.stop();
    rt.block_on(running.shutdown());
}
