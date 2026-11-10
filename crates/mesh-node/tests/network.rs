//! Milestones 1, 2 and 5 as automated tests over real TCP on localhost.

use mesh_crypto::{Digest, Keypair};
use mesh_node::{Config, Event, Node, Running};
use mesh_object::{create, Author, Content};
use mesh_store::Store;
use mesh_transport_tcp::TcpTransport;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

struct TestNode {
    node: Node,
    running: Option<Running>,
    events: broadcast::Receiver<Event>,
    keypair: Keypair,
    _dir: tempfile::TempDir,
}

impl TestNode {
    async fn start(relay: bool, dial: &[&TestNode]) -> TestNode {
        Self::start_with(Config { relay, ..test_config() }, dial).await
    }

    async fn start_with(config: Config, dial: &[&TestNode]) -> TestNode {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for d in dial {
            store.add_peer(d.addr(), None).unwrap();
        }
        let seed = mesh_crypto::random_bytes::<32>();
        let node = Node::new(Keypair::from_seed(&seed), store, Arc::new(TcpTransport), config);
        let events = node.subscribe();
        let running = node.start().await.unwrap();
        TestNode { node, running: Some(running), events, keypair: Keypair::from_seed(&seed), _dir: dir }
    }

    fn addr(&self) -> &str {
        self.running.as_ref().unwrap().local_addr.as_deref().unwrap()
    }

    fn id(&self) -> Digest {
        self.keypair.public().id()
    }

    fn publish(&self, content: Content) -> Digest {
        let obj = create(content, Author::Signed(&self.keypair)).unwrap();
        self.node.publish(&obj).unwrap();
        obj.id()
    }

    async fn stop(&mut self) {
        self.running.take().unwrap().shutdown().await;
    }

    /// Wait until `id` is received, failing after a timeout.
    async fn expect_object(&mut self, id: Digest) -> mesh_object::VerifiedObject {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, self.events.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {}", id.short()))
                .unwrap();
            if let Event::ObjectReceived { object, .. } = ev {
                if object.id() == id {
                    return *object;
                }
            }
        }
    }

    async fn wait_connected(&self, n: usize) {
        for _ in 0..200 {
            if self.node.connected_peers().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never reached {n} peers");
    }
}

#[tokio::test]
async fn milestone1_two_nodes() {
    let a = TestNode::start(true, &[]).await;
    let mut b = TestNode::start(true, &[&a]).await;
    b.wait_connected(1).await;

    let id = a.publish(Content::post("Hello from A"));
    let got = b.expect_object(id).await;
    assert_eq!(got.object().body, "Hello from A");
    assert_eq!(got.object().author.unwrap().id(), a.id());
}

#[tokio::test]
async fn objects_published_before_connecting_are_synced() {
    let a = TestNode::start(true, &[]).await;
    let id = a.publish(Content::post("published while alone"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut b = TestNode::start(true, &[&a]).await;
    b.expect_object(id).await;
}

#[tokio::test]
async fn milestone2_three_nodes_and_reply_path() {
    // A <-> B <-> C, A and C never connect directly.
    let mut b = TestNode::start(true, &[]).await;
    let mut a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_connected(2).await;

    let post = a.publish(Content::post("Hello from A"));
    b.expect_object(post).await;
    c.expect_object(post).await;

    let reply = c.publish(Content::post("Reply from C").reply_to(post));
    let got = a.expect_object(reply).await;
    assert_eq!(got.object().parent, Some(post));
    assert!(a.node.connected_peers().iter().all(|p| *p != c.id()), "A and C must not be directly connected");
}

#[tokio::test]
async fn relay_disabled_node_does_not_propagate() {
    let b = TestNode::start(false, &[]).await;
    let a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_connected(2).await;

    let post = a.publish(Content::post("should stop at B"));
    // B retains it (receiving is fine) but must not offer it to C.
    for _ in 0..40 {
        if b.node.with_store(|s| s.has(&post).unwrap()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(b.node.with_store(|s| s.has(&post).unwrap()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!c.node.with_store(|s| s.has(&post).unwrap()));
    while let Ok(ev) = c.events.try_recv() {
        assert!(!matches!(ev, Event::ObjectReceived { .. }));
    }
}

#[tokio::test]
async fn milestone5_network_survives_origin_loss() {
    let mut b = TestNode::start(true, &[]).await;
    let mut a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_connected(2).await;

    let post = a.publish(Content::post("before A leaves"));
    c.expect_object(post).await;
    a.stop().await;

    let reply = c.publish(Content::post("A is gone, we carry on").reply_to(post));
    b.expect_object(reply).await;
    let from_b = b.publish(Content::post("still here"));
    c.expect_object(from_b).await;
}

/// A discovery medium that reports one fixed sighting — stands in for BLE, NFC, audio, etc.
struct FakeMedium(mesh_discovery::Sighting);
impl mesh_discovery::Discovery for FakeMedium {
    fn medium(&self) -> mesh_discovery::Medium {
        self.0.medium
    }
    fn run(
        self: Arc<Self>,
        _beacon: Option<mesh_discovery::Beacon>,
        out: tokio::sync::mpsc::Sender<mesh_discovery::Sighting>,
    ) -> mesh_transport::BoxFuture<'static, std::io::Result<()>> {
        Box::pin(async move {
            let _ = out.send(self.0.clone()).await;
            std::future::pending::<()>().await;
            Ok(())
        })
    }
}

async fn start_with_sighting(sighting: mesh_discovery::Sighting) -> TestNode {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let seed = mesh_crypto::random_bytes::<32>();
    let config = Config {
        listen: Some("127.0.0.1:0".into()),
        dial_interval: Duration::from_millis(200),
        announce_interval: Duration::from_millis(50),
        cover_bytes_per_hour: 0, // deterministic tests; cover has its own test
        cover_messages_per_hour: 0,
        author_delay: (Duration::ZERO, Duration::ZERO),
        ..Config::default()
    };
    let node = Node::new(Keypair::from_seed(&seed), store, Arc::new(TcpTransport), config);
    node.add_discovery(Arc::new(FakeMedium(sighting)));
    let events = node.subscribe();
    let running = node.start().await.unwrap();
    TestNode { node, running: Some(running), events, keypair: Keypair::from_seed(&seed), _dir: dir }
}

#[tokio::test]
async fn discovery_sighting_leads_to_sync() {
    let a = TestNode::start(true, &[]).await;
    let post = a.publish(Content::post("found you nearby"));
    let mut b = start_with_sighting(mesh_discovery::Sighting {
        medium: mesh_discovery::Medium::Bluetooth,
        addr: a.addr().to_string(),
        identity: None,
    })
    .await;
    b.expect_object(post).await;
}

#[tokio::test]
async fn pinned_identity_mismatch_is_refused() {
    let a = TestNode::start(true, &[]).await;
    let _post = a.publish(Content::post("should never arrive"));
    let mut b = start_with_sighting(mesh_discovery::Sighting {
        medium: mesh_discovery::Medium::Nfc,
        addr: a.addr().to_string(),
        identity: Some(mesh_crypto::Digest::of(b"someone else")),
    })
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, b.events.recv()).await {
            Ok(Ok(Event::DialFailed { reason, .. })) if reason.contains("identity mismatch") => break,
            Ok(Ok(Event::ObjectReceived { .. })) => panic!("accepted object from wrong identity"),
            Ok(Ok(_)) => continue,
            _ => panic!("expected identity mismatch"),
        }
    }
    assert!(b.node.connected_peers().is_empty());
}

fn test_config() -> Config {
    Config {
        listen: Some("127.0.0.1:0".into()),
        dial_interval: Duration::from_millis(200),
        announce_interval: Duration::from_millis(50),
        cover_bytes_per_hour: 0, // deterministic tests; cover has its own test
        cover_messages_per_hour: 0,
        author_delay: (Duration::ZERO, Duration::ZERO),
        ..Config::default()
    }
}

/// Collect events for `dur`.
async fn drain(rx: &mut broadcast::Receiver<Event>, dur: Duration) -> Vec<Event> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + dur;
    while let Ok(Ok(ev)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn anonymous_post_goes_public_from_a_relay_not_the_author() {
    // A — B — C. B always fluffs, so the stem is exactly A → B.
    let mut b = TestNode::start_with(Config { stem_fluff_probability: 1.0, ..test_config() }, &[]).await;
    let mut a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_connected(2).await;

    let obj = create(Content::post("anonymous tip"), Author::Ownerless).unwrap();
    a.node.publish_anonymous(&obj).unwrap();

    // C gets it through the normal public path, from B.
    c.expect_object(obj.id()).await;
    let b_events = drain(&mut b.events, Duration::from_millis(300)).await;
    assert!(b_events.iter().any(|e| matches!(e, Event::Fluffed(id) if *id == obj.id())), "B made it public");

    // A never made it public itself, and has no record that it wrote it.
    let a_events = drain(&mut a.events, Duration::from_millis(300)).await;
    assert!(!a_events.iter().any(|e| matches!(e, Event::Fluffed(id) if *id == obj.id())), "author must not fluff");
    a.node.with_store(|s| {
        assert_eq!(s.origin(&obj.id()).unwrap(), Some(mesh_store::Origin::Stem));
        assert!(s.stem_outbox().unwrap().is_empty(), "outbox entry removed once pushed");
        assert!(!s.is_embargoed(&obj.id()).unwrap(), "seeing it public elsewhere lifts A's embargo");
    });
}

#[tokio::test]
async fn author_without_relay_peers_holds_anonymous_post() {
    // B does not relay, so it does not accept stems. A must not fall back to announcing.
    let b = TestNode::start(false, &[]).await;
    let a = TestNode::start(true, &[&b]).await;
    b.wait_connected(1).await;

    let obj = create(Content::post("waiting for cover"), Author::Ownerless).unwrap();
    a.node.publish_anonymous(&obj).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(!b.node.with_store(|s| s.has(&obj.id()).unwrap()), "no relay peer: nothing leaves A");
    a.node.with_store(|s| {
        assert_eq!(s.stem_outbox().unwrap(), vec![obj.id()]);
        assert!(s.is_embargoed(&obj.id()).unwrap());
    });
}

#[tokio::test]
async fn longer_stem_still_reaches_everyone() {
    // A — B — C — D, nobody fluffs early: the stem runs to D (no further successor), which fluffs.
    let no_early_fluff = || Config { stem_fluff_probability: 0.0, ..test_config() };
    let b = TestNode::start_with(no_early_fluff(), &[]).await;
    let c = TestNode::start_with(no_early_fluff(), &[&b]).await;
    let mut d = TestNode::start_with(no_early_fluff(), &[&c]).await;
    let mut a = TestNode::start_with(no_early_fluff(), &[&b]).await;
    b.wait_connected(2).await;
    c.wait_connected(2).await;

    let obj = create(Content::post("long way round"), Author::Ownerless).unwrap();
    a.node.publish_anonymous(&obj).unwrap();
    d.expect_object(obj.id()).await;
    let d_events = drain(&mut d.events, Duration::from_millis(300)).await;
    assert!(d_events.iter().any(|e| matches!(e, Event::Fluffed(id) if *id == obj.id())), "end of stem fluffs");
    for _ in 0..40 {
        if !a.node.with_store(|s| s.is_embargoed(&obj.id()).unwrap()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!a.node.with_store(|s| s.is_embargoed(&obj.id()).unwrap()), "public result propagates back to A");
    let _ = drain(&mut a.events, Duration::from_millis(10)).await;
}

// ---- Encrypted transport ----

/// TCP proxy that forwards one connection to `target` and records every byte
/// in both directions, as an ISP or Wi-Fi operator would see it.
async fn recording_proxy(target: String) -> (String, Arc<std::sync::Mutex<Vec<u8>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let (client, _) = listener.accept().await.unwrap();
        let server = tokio::net::TcpStream::connect(target).await.unwrap();
        let (mut cr, mut cw) = client.into_split();
        let (mut sr, mut sw) = server.into_split();
        let l2 = log.clone();
        let up = async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = cr.read(&mut buf).await {
                if n == 0 || sw.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                log.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        };
        let down = async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = sr.read(&mut buf).await {
                if n == 0 || cw.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                l2.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        };
        tokio::join!(up, down);
    });
    (addr, seen)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn wire_hides_content_and_identities() {
    let a = TestNode::start(true, &[]).await;
    let body = "Senate inquiry submission: draft wording for the duty of care amendment";
    let post = a.publish(Content::post(body));
    let (proxy, seen) = recording_proxy(a.addr().to_string()).await;

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.add_peer(&proxy, None).unwrap();
    let kb = Keypair::generate();
    let b_pk = kb.public();
    let node = Node::new(kb, store, Arc::new(TcpTransport), test_config());
    let mut events = node.subscribe();
    let running = node.start().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Event::ObjectReceived { object, .. } = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap() {
            if object.id() == post {
                break;
            }
        }
    }
    running.shutdown().await;

    let wire = seen.lock().unwrap().clone();
    assert!(wire.len() > 1000, "proxy should have carried the object");
    for (what, needle) in [
        ("post body", body.as_bytes().to_vec()),
        ("object id", post.to_bytes().to_vec()),
        ("A's public key", a.keypair.public().to_bytes().to_vec()),
        ("B's public key", b_pk.to_bytes().to_vec()),
        ("A's identity id", a.id().to_bytes().to_vec()),
        ("object magic", b"SMSH".to_vec()),
    ] {
        assert!(!contains(&wire, &needle), "{what} visible to a network observer");
    }
}

/// A pinned dial that reaches the wrong node must hang up before saying who it is.
#[tokio::test]
async fn pinned_dial_to_impostor_reveals_nothing() {
    use mesh_protocol::secure::{secure, Role};
    use tokio::io::AsyncReadExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let impostor_addr = listener.local_addr().unwrap().to_string();
    let impostor = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let ch = secure(sock, Role::Responder).await.unwrap();
        let (mut rd, mut wr) = tokio::io::split(ch.stream);
        let k = Keypair::generate();
        // A valid HELLO for the impostor's own key: honest proof, wrong identity.
        let pk = k.public();
        let sig = k.sign(&mesh_protocol::hello_input(&ch.binding, Role::Responder, mesh_protocol::PROTOCOL_VERSION, 1, &pk));
        let hello = mesh_protocol::Message::Hello { protocol: mesh_protocol::PROTOCOL_VERSION, flags: 1, public_key: pk, signature: sig };
        mesh_protocol::write_message(&mut wr, &hello).await.unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), rd.read_to_end(&mut got)).await;
        got
    });

    let mut b = start_with_sighting(mesh_discovery::Sighting {
        medium: mesh_discovery::Medium::Nfc,
        addr: impostor_addr,
        identity: Some(Digest::of(b"the node we were introduced to")),
    })
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, b.events.recv()).await {
            Ok(Ok(Event::DialFailed { reason, .. })) if reason.contains("identity mismatch") => break,
            Ok(Ok(_)) => continue,
            _ => panic!("expected identity mismatch"),
        }
    }
    let received = impostor.await.unwrap();
    assert!(received.is_empty(), "dialler sent {} plaintext bytes to an impostor", received.len());
}

#[tokio::test]
async fn probe_learns_identity_without_revealing_ours() {
    let mut a = TestNode::start(true, &[]).await;
    let me = Keypair::generate();
    let pk = mesh_node::probe(&TcpTransport, &me, a.addr(), Duration::from_secs(5)).await.unwrap();
    assert_eq!(pk.id(), a.id());
    for ev in drain(&mut a.events, Duration::from_millis(300)).await {
        if let Event::PeerConnected { peer, .. } = ev {
            panic!("probed node learned prober identity {}", peer.short());
        }
    }
}

// ---- Milestone 4: attachments ----

use mesh_store::{BlobSource, WantedAttachment};

fn attachment(w: &WantedAttachment) -> mesh_object::Attachment {
    mesh_object::Attachment { object: w.manifest, key: w.key, media_type: "image/png".into(), size: w.size }
}

fn test_file(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8).collect()
}

impl TestNode {
    /// Publish a post with one attachment. Returns (post id, manifest id).
    fn publish_with_file(&self, text: &str, data: &[u8]) -> (Digest, Digest) {
        let w = self.node.with_store(|s| s.add_file(data, BlobSource::Own)).unwrap();
        (self.publish(Content::post(text).attach(attachment(&w))), w.manifest)
    }

    async fn expect_attachment(&mut self, manifest: Digest) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Ok(Event::AttachmentComplete { manifest: m })) if m == manifest => return,
                Ok(Ok(Event::AttachmentFailed { manifest: m, reason })) if m == manifest => panic!("failed: {reason}"),
                Ok(Ok(_)) => continue,
                _ => panic!("timed out waiting for attachment {}", manifest.short()),
            }
        }
    }

    /// Decrypt and reassemble an attachment of `post`.
    fn read_attachment(&self, post: Digest) -> Vec<u8> {
        let obj = self.node.with_store(|s| s.get(&post).unwrap().unwrap());
        let w: WantedAttachment = (&obj.object().attachments[0]).into();
        let mut out = Vec::new();
        self.node.with_store(|s| s.write_attachment(&w, &mut out)).unwrap();
        out
    }
}

/// README Milestone 4: A posts text + image; C, connected only to B, receives
/// the post through B, requests the attachment, and gets the chunks through B.
#[tokio::test]
async fn milestone4_attachment_through_relay() {
    let mut b = TestNode::start(true, &[]).await;
    let a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(false, &[&b]).await;
    b.wait_connected(2).await;

    let image = test_file(300_000, 1); // 5 chunks
    let (post, manifest) = a.publish_with_file("photo from the rally", &image);

    let got = c.expect_object(post).await;
    assert_eq!(got.object().attachments[0].object, manifest);
    // B relays, so it fetched the file by itself to keep it available.
    b.expect_attachment(manifest).await;
    // C does not relay: it fetches only when asked.
    assert!(c.node.with_store(|s| s.attachment_state(&manifest).unwrap()).is_none());
    c.node.fetch_attachments(&got).unwrap();
    c.expect_attachment(manifest).await;
    assert_eq!(c.read_attachment(post), image);
    assert!(c.node.connected_peers().iter().all(|p| *p != a.id()));
}

#[tokio::test]
async fn attachment_chunks_come_from_several_peers() {
    let a = TestNode::start(true, &[]).await;
    let mut b = TestNode::start(true, &[&a]).await;
    let data = test_file(2 * 1024 * 1024, 2); // 32 chunks
    let (post, manifest) = a.publish_with_file("big file", &data);
    b.expect_attachment(manifest).await;

    let mut c = TestNode::start(false, &[&a, &b]).await;
    c.wait_connected(2).await;
    let obj = c.expect_object(post).await;
    c.node.fetch_attachments(&obj).unwrap();
    c.expect_attachment(manifest).await;
    assert_eq!(c.read_attachment(post), data);

    let sources = c.node.blob_sources();
    assert_eq!(sources.len(), 2, "chunks should be spread across both peers: {sources:?}");
    assert!(sources.values().all(|n| *n >= 4), "both peers did real work: {sources:?}");
}

/// A peer that answers every blob request with junk: its replies are rejected
/// on arrival, it takes strikes, and the file still completes from an honest peer.
#[tokio::test]
async fn lying_peer_cannot_corrupt_an_attachment() {
    use mesh_protocol::secure::{secure, Role};
    use mesh_protocol::{hello_input, read_message, write_message, Message, PROTOCOL_VERSION};

    let a = TestNode::start(true, &[]).await;
    let data = test_file(400_000, 3);
    let (post, manifest) = a.publish_with_file("evidence", &data);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let liar_addr = listener.local_addr().unwrap().to_string();
    let liar = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let ch = secure(sock, Role::Responder).await.unwrap();
        let (mut rd, mut wr) = tokio::io::split(ch.stream);
        let k = Keypair::generate();
        let sig = k.sign(&hello_input(&ch.binding, Role::Responder, PROTOCOL_VERSION, 1, &k.public()));
        write_message(&mut wr, &Message::Hello { protocol: PROTOCOL_VERSION, flags: 1, public_key: k.public(), signature: sig })
            .await
            .unwrap();
        let mut lies = 0;
        while let Ok(Some(m)) = read_message(&mut rd).await {
            if let Message::BlobWant(ids) = m {
                for _ in ids {
                    lies += 1;
                    if write_message(&mut wr, &Message::Blob(vec![0xEE; mesh_object::blob::BLOB_SIZE])).await.is_err() {
                        return lies;
                    }
                }
            }
        }
        lies
    });

    let mut c = TestNode::start(false, &[&a]).await;
    c.node.with_store(|s| s.add_peer(&liar_addr, None)).unwrap();
    c.wait_connected(2).await;
    let obj = c.expect_object(post).await;
    c.node.fetch_attachments(&obj).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut rejected = 0;
    loop {
        match tokio::time::timeout_at(deadline, c.events.recv()).await {
            Ok(Ok(Event::AttachmentComplete { manifest: m })) if m == manifest => break,
            Ok(Ok(Event::ObjectRejected { reason, .. })) if reason.contains("unsolicited blob") => rejected += 1,
            Ok(Ok(Event::AttachmentFailed { reason, .. })) => panic!("failed: {reason}"),
            Ok(Ok(_)) => continue,
            _ => panic!("timed out"),
        }
    }
    assert_eq!(c.read_attachment(post), data);
    c.stop().await;
    let lies = liar.await.unwrap();
    assert!(lies > 0, "the liar should have been asked for something");
    assert!(rejected > 0, "junk must be rejected");
}

#[tokio::test]
async fn anonymous_attachment_travels_the_stem_and_is_held_by_relays() {
    // A — B — C. B always fluffs, so the stem is exactly A → B.
    let mut b = TestNode::start_with(Config { stem_fluff_probability: 1.0, ..test_config() }, &[]).await;
    let a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_connected(2).await;

    let leak = test_file(300_000, 9);
    let w = a.node.with_store(|s| s.add_file(&leak, BlobSource::Cache)).unwrap();
    let obj = create(Content::post("leaked document").attach(attachment(&w)), Author::Ownerless).unwrap();
    a.node.publish_anonymous(&obj).unwrap();

    // B received every sealed blob *before* the post (same ordered queue), so
    // it held the whole file at the moment it made the post public.
    let mut fluffed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !fluffed {
        if let Event::Fluffed(id) = tokio::time::timeout_at(deadline, b.events.recv()).await.unwrap().unwrap() {
            fluffed = id == obj.id();
        }
    }
    let blobs = b.node.with_store(|s| s.attachment_blobs(&w).unwrap());
    assert_eq!(blobs.len(), 6);
    assert!(b.node.with_store(|s| blobs.iter().all(|id| s.has_blob(id).unwrap())), "relay holds the whole file");

    // C, connected only to B, gets the post and the file without ever touching A.
    let got = c.expect_object(obj.id()).await;
    c.node.fetch_attachments(&got).unwrap();
    c.expect_attachment(w.manifest).await;
    assert_eq!(c.read_attachment(obj.id()), leak);

    // Pacing delays the hand-over, but the author's fallback timer only starts
    // after it, so the author never makes its own post public.
    let mut a = a;
    for ev in drain(&mut a.events, Duration::from_millis(200)).await {
        assert!(!matches!(ev, Event::Fluffed(id) if id == obj.id()), "author fluffed its own post");
    }
    assert!(a.node.with_store(|s| s.stem_outbox().unwrap().is_empty()));

    // On disk, the author's copy looks exactly like the relay's.
    for node in [&a, &b] {
        node.node.with_store(|s| {
            for id in &blobs {
                assert_eq!(s.blob_source(id).unwrap(), Some(BlobSource::Cache));
            }
        });
    }
}

#[tokio::test]
async fn anonymous_attachment_stored_as_own_is_refused() {
    let a = TestNode::start(true, &[]).await;
    let w = a.node.with_store(|s| s.add_file(b"scan", BlobSource::Own)).unwrap();
    let obj = create(Content::post("anon").attach(attachment(&w)), Author::Ownerless).unwrap();
    assert!(a.node.publish_anonymous(&obj).is_err(), "own blobs would mark this node as the author");
}

#[tokio::test]
async fn non_relay_node_rejects_stash() {
    // Real nodes never stem to a non-relay node, so push a STASH by hand.
    let a = TestNode::start(false, &[]).await;
    use mesh_protocol::secure::{secure, Role};
    use mesh_protocol::{hello_input, read_message, write_message, Message, PROTOCOL_VERSION};
    let sock = tokio::net::TcpStream::connect(a.addr()).await.unwrap();
    let ch = secure(sock, Role::Initiator).await.unwrap();
    let (mut rd, mut wr) = tokio::io::split(ch.stream);
    assert!(matches!(read_message(&mut rd).await.unwrap(), Some(Message::Hello { .. })));
    let k = Keypair::generate();
    let sig = k.sign(&hello_input(&ch.binding, Role::Initiator, PROTOCOL_VERSION, 1, &k.public()));
    write_message(&mut wr, &Message::Hello { protocol: PROTOCOL_VERSION, flags: 1, public_key: k.public(), signature: sig })
        .await
        .unwrap();
    let blob = vec![1u8; mesh_object::blob::BLOB_SIZE];
    write_message(&mut wr, &Message::Stash(blob.clone())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!a.node.with_store(|s| s.has_blob(&Digest::of(&blob)).unwrap()));
}

// ---- Cover traffic and cache budget ----

#[tokio::test]
async fn cover_traffic_flows_and_is_absorbed_as_cache() {
    let mut b = TestNode::start(true, &[]).await;
    // Very high rate so runs start almost immediately in the test.
    let a = TestNode::start_with(
        Config { cover_bytes_per_hour: 1 << 40, cover_max_run: 4, ..test_config() },
        &[&b],
    )
    .await;
    b.wait_connected(1).await;
    let mut held = 0;
    for _ in 0..100 {
        held = b.node.with_store(|s| s.cache_bytes().unwrap());
        if held >= 8 * mesh_object::blob::BLOB_SIZE as u64 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(held >= 8 * mesh_object::blob::BLOB_SIZE as u64, "B should hold decoy pieces, has {held} bytes");
    // Decoys are indistinguishable, so B neither rejects them nor drops A.
    for ev in drain(&mut b.events, Duration::from_millis(200)).await {
        assert!(!matches!(ev, Event::ObjectRejected { .. } | Event::PeerDisconnected { .. }), "{ev:?}");
    }
    assert_eq!(b.node.connected_peers(), vec![a.id()]);
}

// ---- Several media at once ----

/// A second medium for tests: TCP under another name, so it gets its own
/// links, listener and `alt:` addresses, like Bluetooth or Wi-Fi Direct would.
struct AltMedium;

impl mesh_transport::Transport for AltMedium {
    fn name(&self) -> &'static str {
        "alt"
    }
    fn dial<'a>(&'a self, addr: &'a str) -> mesh_transport::BoxFuture<'a, std::io::Result<mesh_transport::Stream>> {
        TcpTransport.dial(addr)
    }
    fn listen<'a>(
        &'a self,
        addr: &'a str,
    ) -> mesh_transport::BoxFuture<'a, std::io::Result<Box<dyn mesh_transport::Listener>>> {
        TcpTransport.listen(addr)
    }
}

impl TestNode {
    /// Listens on TCP and on the `alt` medium; dials every address in `dial`.
    async fn start_two_media(config: Config, dial: &[String]) -> TestNode {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for d in dial {
            store.add_peer(d, None).unwrap();
        }
        let seed = mesh_crypto::random_bytes::<32>();
        let node = Node::new(Keypair::from_seed(&seed), store, Arc::new(TcpTransport), config);
        node.add_transport(Arc::new(AltMedium), Some("127.0.0.1:0".into()));
        let events = node.subscribe();
        let running = node.start().await.unwrap();
        TestNode { node, running: Some(running), events, keypair: Keypair::from_seed(&seed), _dir: dir }
    }

    /// Both of this node's addresses: plain TCP and `alt:...`.
    fn both_addrs(&self) -> Vec<String> {
        self.running.as_ref().unwrap().listening.clone()
    }

    fn media_to(&self, peer: Digest) -> Vec<String> {
        self.node.links().into_iter().filter(|(p, _)| *p == peer).map(|(_, m)| m).collect()
    }

    async fn wait_media(&self, peer: Digest, n: usize) {
        for _ in 0..200 {
            if self.media_to(peer).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never reached {n} links to {}: {:?}", peer.short(), self.media_to(peer));
    }
}

#[tokio::test]
async fn peer_is_linked_over_every_shared_medium() {
    let a = TestNode::start_two_media(test_config(), &[]).await;
    assert_eq!(a.both_addrs().len(), 2);
    assert!(a.both_addrs()[1].starts_with("alt:"));
    let mut b = TestNode::start_two_media(test_config(), &a.both_addrs()).await;
    b.wait_media(a.id(), 2).await;
    a.wait_media(b.id(), 2).await;
    assert_eq!(b.media_to(a.id()), vec!["alt".to_string(), "tcp".to_string()]);
    assert_eq!(b.node.connected_peers(), vec![a.id()], "still one peer");

    // Stable: no redialling churn once linked.
    let _ = drain(&mut b.events, Duration::from_millis(100)).await;
    let later = drain(&mut b.events, Duration::from_millis(1000)).await;
    assert!(
        !later.iter().any(|e| matches!(e, Event::PeerConnected { .. } | Event::PeerDisconnected { .. })),
        "{later:?}"
    );

    // A post still arrives.
    let post = a.publish(Content::post("two roads"));
    b.expect_object(post).await;
}

#[tokio::test]
async fn both_ends_dialling_keep_one_link_per_medium() {
    let a = TestNode::start_two_media(test_config(), &[]).await;
    let b = TestNode::start_two_media(test_config(), &a.both_addrs()).await;
    for addr in b.both_addrs() {
        a.node.dial(&addr, None);
    }
    a.wait_media(b.id(), 2).await;
    b.wait_media(a.id(), 2).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(a.media_to(b.id()), vec!["alt".to_string(), "tcp".to_string()]);
    assert_eq!(b.media_to(a.id()), vec!["alt".to_string(), "tcp".to_string()]);
}

#[tokio::test]
async fn attachment_arrives_over_every_medium() {
    let a = TestNode::start_two_media(test_config(), &[]).await;
    let mut b = TestNode::start_two_media(Config { relay: false, ..test_config() }, &a.both_addrs()).await;
    b.wait_media(a.id(), 2).await;
    let data = test_file(2 * 1024 * 1024, 5); // 32 chunks
    let (post, manifest) = a.publish_with_file("over two media", &data);
    let obj = b.expect_object(post).await;
    b.node.fetch_attachments(&obj).unwrap();
    b.expect_attachment(manifest).await;
    assert_eq!(b.read_attachment(post), data);

    let by = b.node.pieces_by_medium();
    assert!(by.get("tcp").copied().unwrap_or(0) >= 4 && by.get("alt").copied().unwrap_or(0) >= 4, "{by:?}");
}

#[tokio::test]
async fn anonymous_attachment_is_split_across_media_and_reassembled() {
    // Author A — relay B over both media; C — B over TCP only. B always fluffs.
    let mut b = TestNode::start_two_media(Config { stem_fluff_probability: 1.0, ..test_config() }, &[]).await;
    let a = TestNode::start_two_media(test_config(), &b.both_addrs()).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_media(a.id(), 2).await;
    b.wait_connected(2).await;

    let leak = test_file(1024 * 1024, 11); // 18 sealed pieces with the manifest
    let w = a.node.with_store(|s| s.add_file(&leak, BlobSource::Cache)).unwrap();
    let obj = create(Content::post("split leak").attach(attachment(&w)), Author::Ownerless).unwrap();
    a.node.publish_anonymous(&obj).unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Event::Fluffed(id) = tokio::time::timeout_at(deadline, b.events.recv()).await.unwrap().unwrap() {
            if id == obj.id() {
                break;
            }
        }
    }
    // B held the whole file when it went public, and got it over both media.
    let blobs = b.node.with_store(|s| s.attachment_blobs(&w).unwrap());
    assert!(b.node.with_store(|s| blobs.iter().all(|id| s.has_blob(id).unwrap())));
    let by = b.node.pieces_by_medium();
    assert!(by.get("tcp").copied().unwrap_or(0) >= 1 && by.get("alt").copied().unwrap_or(0) >= 1, "{by:?}");
    assert_eq!(by.values().sum::<u64>(), blobs.len() as u64, "{by:?}");

    let got = c.expect_object(obj.id()).await;
    c.node.fetch_attachments(&got).unwrap();
    c.expect_attachment(w.manifest).await;
    assert_eq!(c.read_attachment(obj.id()), leak);
}

#[tokio::test]
async fn relay_waits_for_pieces_still_on_a_slower_link() {
    // A STEM that overtakes its pieces must not end the route early.
    use mesh_protocol::secure::{secure, Role};
    use mesh_protocol::{hello_input, read_message, write_message, Message, PROTOCOL_VERSION};

    // B relays and never fluffs early; D is B's only other peer.
    let mut b = TestNode::start_with(Config { stem_fluff_probability: 0.0, ..test_config() }, &[]).await;
    let mut d = TestNode::start_with(Config { stem_fluff_probability: 1.0, ..test_config() }, &[&b]).await;
    b.wait_connected(1).await;

    // A sealed file and its anonymous post, prepared in a scratch store.
    let dir = tempfile::tempdir().unwrap();
    let scratch = Store::open(dir.path()).unwrap();
    let leak = test_file(300_000, 21);
    let w = scratch.add_file(&leak, BlobSource::Cache).unwrap();
    let obj = create(Content::post("late pieces").attach(attachment(&w)), Author::Ownerless).unwrap();
    let pieces: Vec<Vec<u8>> =
        scratch.attachment_blobs(&w).unwrap().iter().map(|id| scratch.get_blob(id).unwrap().unwrap()).collect();

    // Hand-rolled author: STEM first, pieces a second later.
    let sock = tokio::net::TcpStream::connect(b.addr()).await.unwrap();
    let ch = secure(sock, Role::Initiator).await.unwrap();
    let (mut rd, mut wr) = tokio::io::split(ch.stream);
    assert!(matches!(read_message(&mut rd).await.unwrap(), Some(Message::Hello { .. })));
    let k = Keypair::generate();
    let sig = k.sign(&hello_input(&ch.binding, Role::Initiator, PROTOCOL_VERSION, 1, &k.public()));
    write_message(&mut wr, &Message::Hello { protocol: PROTOCOL_VERSION, flags: 1, public_key: k.public(), signature: sig })
        .await
        .unwrap();
    b.wait_connected(2).await;
    write_message(&mut wr, &Message::Stem(obj.bytes().to_vec())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let early = drain(&mut b.events, Duration::from_millis(10)).await;
    assert!(!early.iter().any(|e| matches!(e, Event::Fluffed(id) if *id == obj.id())), "B ended the stem early");
    for p in pieces {
        write_message(&mut wr, &Message::Stash(p)).await.unwrap();
    }

    // B passes post and file on to D, which makes it public holding the file.
    d.expect_object(obj.id()).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Event::Fluffed(id) = tokio::time::timeout_at(deadline, d.events.recv()).await.unwrap().unwrap() {
            if id == obj.id() {
                break;
            }
        }
    }
    let blobs = d.node.with_store(|s| s.attachment_blobs(&w).unwrap());
    assert!(d.node.with_store(|s| blobs.iter().all(|id| s.has_blob(id).unwrap())), "D holds the whole file");
    drop(wr);
}

// ---- Link liveness and link keys ----

/// Connect to `addr` by hand and complete the handshake as `key`, relaying.
async fn raw_link(
    addr: &str,
    key: &Keypair,
) -> (
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
) {
    use mesh_protocol::secure::{secure, Role};
    use mesh_protocol::{hello_input, read_message, write_message, Message, PROTOCOL_VERSION};
    let sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let ch = secure(sock, Role::Initiator).await.unwrap();
    let (mut rd, mut wr) = tokio::io::split(ch.stream);
    assert!(matches!(read_message(&mut rd).await.unwrap(), Some(Message::Hello { .. })));
    let sig = key.sign(&hello_input(&ch.binding, Role::Initiator, PROTOCOL_VERSION, 1, &key.public()));
    write_message(&mut wr, &Message::Hello { protocol: PROTOCOL_VERSION, flags: 1, public_key: key.public(), signature: sig })
        .await
        .unwrap();
    (rd, wr)
}

/// Read until the link closes. Panics if it stays open for `within`.
async fn expect_closed<R: tokio::io::AsyncRead + Unpin>(rd: &mut R, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, mesh_protocol::read_message(rd)).await {
            Ok(Ok(Some(_))) => continue,
            Ok(_) => return, // closed or broken
            Err(_) => panic!("link still open"),
        }
    }
}

#[tokio::test]
async fn silent_link_is_dropped() {
    let mut a = TestNode::start_with(
        Config { keepalive: Duration::from_millis(200), link_timeout: Duration::from_millis(1000), ..test_config() },
        &[],
    )
    .await;
    let k = Keypair::generate();
    // A peer that completes the handshake and then says nothing at all, as one
    // that walked out of radio range would.
    let (mut rd, _wr) = raw_link(a.addr(), &k).await;
    a.wait_connected(1).await;

    // We keep reading, so A's own keepalives get through; ours never come.
    expect_closed(&mut rd, Duration::from_secs(5)).await;
    assert!(a.node.connected_peers().is_empty());
    let evs = drain(&mut a.events, Duration::from_millis(50)).await;
    assert!(
        evs.iter().any(|e| matches!(e, Event::PeerDisconnected { reason: Some(r), .. } if r.contains("silent"))),
        "{evs:?}"
    );
}

#[tokio::test]
async fn keepalives_hold_an_idle_link_open() {
    let quick = || Config { keepalive: Duration::from_millis(100), link_timeout: Duration::from_millis(600), ..test_config() };
    let a = TestNode::start_with(quick(), &[]).await;
    let mut b = TestNode::start_with(quick(), &[&a]).await;
    b.wait_connected(1).await;
    // Many timeouts' worth of idleness: keepalives keep the link up.
    let evs = drain(&mut b.events, Duration::from_millis(2000)).await;
    assert!(!evs.iter().any(|e| matches!(e, Event::PeerDisconnected { .. })), "{evs:?}");
    assert_eq!(b.node.connected_peers(), vec![a.id()]);
}

#[tokio::test]
async fn returning_peer_replaces_its_stale_link() {
    let a = TestNode::start_with(
        Config {
            keepalive: Duration::from_millis(200),
            link_race_window: Duration::from_millis(200),
            ..test_config() // long link_timeout: A hasn't noticed the old link is dead
        },
        &[],
    )
    .await;
    let k = Keypair::generate();
    let (mut old_rd, _old_wr) = raw_link(a.addr(), &k).await;
    a.wait_connected(1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // The same peer comes back over the same medium and dials again.
    let (mut new_rd, _new_wr) = raw_link(a.addr(), &k).await;
    expect_closed(&mut old_rd, Duration::from_secs(3)).await;
    assert_eq!(a.node.links(), vec![(k.public().id(), "tcp".to_string())]);
    // The new link is the live one: A's keepalives arrive on it.
    let m = tokio::time::timeout(Duration::from_secs(2), mesh_protocol::read_message(&mut new_rd)).await;
    assert!(matches!(m, Ok(Ok(Some(_)))), "{m:?}");
}

#[tokio::test]
async fn rotating_the_link_key_reconnects_as_a_stranger() {
    let mut a = TestNode::start(true, &[]).await;
    let b = TestNode::start(true, &[&a]).await;
    a.wait_connected(1).await;
    let old = b.node.identity().id();
    assert_eq!(a.node.connected_peers(), vec![old]);

    b.node.rotate_link_key();
    let new = b.node.identity().id();
    assert_ne!(old, new);
    for _ in 0..200 {
        if a.node.connected_peers() == vec![new] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // A never holds both keys at once, and only knows the new one now.
    assert_eq!(a.node.connected_peers(), vec![new]);

    // Signed posts are untouched: the posting identity was never the link key.
    let author = Keypair::generate();
    let obj = create(Content::post("still me"), Author::Signed(&author)).unwrap();
    b.node.publish(&obj).unwrap();
    a.expect_object(obj.id()).await;
}

// ---- Private origin for signed posts ----

#[tokio::test]
async fn signed_post_with_file_goes_public_from_a_relay_not_its_author() {
    // A — B — C. B always fluffs. A signs the post, but sends it the private way.
    let mut b = TestNode::start_with(Config { stem_fluff_probability: 1.0, ..test_config() }, &[]).await;
    let mut a = TestNode::start(true, &[&b]).await;
    let mut c = TestNode::start(true, &[&b]).await;
    b.wait_connected(2).await;

    let author = Keypair::generate(); // posting identity: not A's link key
    assert_ne!(author.public().id(), a.id());
    let photo = test_file(200_000, 31);
    let w = a.node.with_store(|s| s.add_file(&photo, BlobSource::Cache)).unwrap();
    let obj = create(Content::post("signed, from somewhere").attach(attachment(&w)), Author::Signed(&author)).unwrap();
    a.node.publish_private(&obj).unwrap();

    // C gets it, with a valid signature by the author, from B.
    let got = c.expect_object(obj.id()).await;
    assert_eq!(got.object().author.unwrap().id(), author.public().id());
    let b_events = drain(&mut b.events, Duration::from_millis(300)).await;
    assert!(b_events.iter().any(|e| matches!(e, Event::Fluffed(id) if *id == obj.id())), "B made it public");
    c.node.fetch_attachments(&got).unwrap();
    c.expect_attachment(w.manifest).await;
    assert_eq!(c.read_attachment(obj.id()), photo);

    // A never announced it, and holds it exactly as a relay would.
    let a_events = drain(&mut a.events, Duration::from_millis(300)).await;
    assert!(!a_events.iter().any(|e| matches!(e, Event::Fluffed(id) if *id == obj.id())), "author must not fluff");
    a.node.with_store(|s| {
        assert_eq!(s.origin(&obj.id()).unwrap(), Some(mesh_store::Origin::Stem));
        for id in s.attachment_blobs(&w).unwrap() {
            assert_eq!(s.blob_source(&id).unwrap(), Some(BlobSource::Cache));
        }
    });
}

#[tokio::test]
async fn author_holds_a_new_post_for_a_random_delay() {
    let mut b = TestNode::start_with(Config { stem_fluff_probability: 1.0, ..test_config() }, &[]).await;
    let a = TestNode::start_with(
        Config { author_delay: (Duration::from_millis(1200), Duration::from_millis(1200)), ..test_config() },
        &[&b],
    )
    .await;
    b.wait_connected(1).await;
    let obj = create(Content::post("not yet"), Author::Ownerless).unwrap();
    a.node.publish_private(&obj).unwrap();

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!b.node.with_store(|s| s.has(&obj.id()).unwrap()), "left before the author delay");
    b.expect_object(obj.id()).await;
}

#[tokio::test]
async fn decoy_messages_flow_to_relaying_peers() {
    use mesh_protocol::{read_message, Message};
    // Keepalives effectively off, decoys about every 100 ms.
    let a = TestNode::start_with(
        Config { cover_messages_per_hour: 36_000, keepalive: Duration::from_secs(60), ..test_config() },
        &[],
    )
    .await;
    let (mut rd, _wr) = raw_link(a.addr(), &Keypair::generate()).await;
    let mut pads = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while let Ok(Ok(Some(m))) = tokio::time::timeout_at(deadline, read_message(&mut rd)).await {
        if let Message::Pad(p) = m {
            assert!((200..=4096).contains(&p.len()), "post-sized: {}", p.len());
            pads += 1;
        }
    }
    assert!(pads >= 4, "only {pads} decoys");
}
