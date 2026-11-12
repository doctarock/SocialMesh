//! Nodes talking over Bluetooth LE only, through the simulated air.

use mesh_crypto::{Digest, Keypair};
use mesh_node::{Config, Event, Node, Running};
use mesh_object::{create, Attachment, Author, Content};
use mesh_store::{AttachmentState, BlobSource, Store};
use mesh_transport_ble::sim::{SimAir, SimConfig, SimDevice};
use mesh_transport_ble::{advert, BleDiscovery, BleTransport, Command};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

struct Phone {
    node: Node,
    _running: Running,
    events: broadcast::Receiver<Event>,
    keypair: Keypair,
    device: SimDevice,
    _dir: tempfile::TempDir,
}

fn config() -> Config {
    Config {
        listen: Some("radio".into()),
        dial_interval: Duration::from_millis(200),
        announce_interval: Duration::from_millis(50),
        cover_bytes_per_hour: 0,
        cover_messages_per_hour: 0,
        author_delay: (Duration::ZERO, Duration::ZERO),
        ..Config::default()
    }
}

/// A node whose only medium is Bluetooth: the phone case.
async fn phone(air: &SimAir, x: f64, y: f64, config: Config) -> Phone {
    let (radio, device) = air.add(x, y);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let seed = mesh_crypto::random_bytes::<32>();
    let node = Node::new(Keypair::from_seed(&seed), store, Arc::new(BleTransport(radio.clone())), config);
    node.add_discovery(Arc::new(BleDiscovery(radio)));
    let events = node.subscribe();
    let running = node.start().await.unwrap();
    Phone { node, _running: running, events, keypair: Keypair::from_seed(&seed), device, _dir: dir }
}

impl Phone {
    fn id(&self) -> Digest {
        self.keypair.public().id()
    }

    fn publish(&self, text: &str) -> Digest {
        let obj = create(Content::post(text), Author::Signed(&self.keypair)).unwrap();
        self.node.publish(&obj).unwrap();
        obj.id()
    }

    fn has(&self, id: &Digest) -> bool {
        self.node.with_store(|s| s.has(id).unwrap())
    }

    async fn expect_object(&mut self, id: Digest) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !self.has(&id) {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Err(_) => panic!("timed out waiting for {}", id.short()),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(r) => {
                    r.unwrap();
                }
            }
        }
    }

    async fn wait_links(&self, n: usize) {
        for _ in 0..300 {
            if self.node.links().len() == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("expected {n} links, have {:?}", self.node.links());
    }
}

#[tokio::test]
async fn phones_in_range_find_each_other_and_sync() {
    let air = SimAir::new(SimConfig::default());
    let a = phone(&air, 0.0, 0.0, config()).await;
    let mut b = phone(&air, 10.0, 0.0, config()).await;

    // Nobody was told about anybody: they meet over adverts alone.
    b.wait_links(1).await;
    assert_eq!(b.node.links(), vec![(a.id(), "ble".to_string())]);
    let post = a.publish("heard it over Bluetooth");
    b.expect_object(post).await;

    // Both advertise, but only one end connected: one radio link.
    assert_eq!(a.device.link_count(), 1);
    assert_eq!(b.device.link_count(), 1);
}

#[tokio::test]
async fn adverts_carry_no_identity() {
    let air = SimAir::new(SimConfig::default());
    let a = phone(&air, 0.0, 0.0, config()).await;
    for _ in 0..100 {
        if a.device.advert().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let data = a.device.advert().expect("advertising");
    assert_eq!(data.len(), advert::LEN);
    let id = a.id();
    let pk = a.node.identity();
    let (id, pk) = (id.to_bytes(), pk.to_bytes());
    for needle in [&id[..4], &id[id.len() - 4..], &pk[..4], &pk[pk.len() - 4..]] {
        assert!(!data.windows(4).any(|w| w == needle), "advert leaks an identity");
    }
}

#[tokio::test]
async fn a_post_crosses_three_phones_out_of_range_of_each_other() {
    // A ... B ... C in a line, 25 apart, range 30: A and C can't hear each other.
    let air = SimAir::new(SimConfig::default());
    let a = phone(&air, 0.0, 0.0, config()).await;
    let b = phone(&air, 25.0, 0.0, config()).await;
    let mut c = phone(&air, 50.0, 0.0, config()).await;
    b.wait_links(2).await;

    let post = a.publish("passed hand to hand");
    c.expect_object(post).await;
    assert!(c.node.links().iter().all(|(p, _)| *p != a.id()), "A and C must not be linked");
}

#[tokio::test]
async fn walking_away_drops_the_link_and_coming_back_catches_up() {
    let air = SimAir::new(SimConfig::default());
    let a = phone(&air, 0.0, 0.0, config()).await;
    let mut b = phone(&air, 10.0, 0.0, config()).await;
    b.wait_links(1).await;

    b.device.move_to(500.0, 0.0);
    b.wait_links(0).await;
    a.wait_links(0).await;

    let post = a.publish("written while you were away");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!b.has(&post));

    b.device.move_to(5.0, 0.0);
    b.expect_object(post).await;
}

#[tokio::test]
async fn listen_only_phone_is_never_heard_but_still_joins() {
    let air = SimAir::new(SimConfig::default());
    let a = phone(&air, 0.0, 0.0, config()).await;
    let mut quiet = phone(&air, 10.0, 0.0, Config { beacon: false, ..config() }).await;

    quiet.wait_links(1).await;
    let post = a.publish("for whoever is listening");
    quiet.expect_object(post).await;

    assert!(quiet.device.advert().is_none());
    assert!(
        !quiet.device.commands().iter().any(|c| matches!(c, Command::Advertise { .. })),
        "a listen-only radio must never advertise"
    );
}

#[tokio::test]
async fn rotating_the_link_key_changes_the_radio_address_and_advert() {
    let air = SimAir::new(SimConfig::default());
    let a = phone(&air, 0.0, 0.0, config()).await;
    let b = phone(&air, 10.0, 0.0, config()).await;
    b.wait_links(1).await;
    let (old_addr, old_advert) = (a.device.address(), a.device.advert().unwrap());
    let old_id = a.node.identity().id();

    a.node.rotate_link_key();
    let new_id = a.node.identity().id();
    assert_ne!(old_id, new_id);
    for _ in 0..100 {
        if a.device.address() != old_addr {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(a.device.address(), old_addr, "hardware address must change with the link key");
    for _ in 0..100 {
        if a.device.advert().is_some_and(|d| d != old_advert) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(a.device.advert().unwrap(), old_advert, "advert token must change too");

    // B finds the "new" node at its new address.
    for _ in 0..300 {
        if b.node.links().iter().any(|(p, _)| *p == new_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("B never met A's new link key: {:?}", b.node.links());
}

#[tokio::test]
async fn attachments_cross_a_slow_small_packet_link_intact() {
    // 64-byte packets, 200 KB/s: the stream must be cut and rejoined exactly.
    let air = SimAir::new(SimConfig { mtu: 64, bytes_per_second: Some(200_000), ..SimConfig::default() });
    let a = phone(&air, 0.0, 0.0, config()).await;
    let mut b = phone(&air, 10.0, 0.0, config()).await;
    b.wait_links(1).await;

    let data: Vec<u8> = (0..150_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let w = a.node.with_store(|s| s.add_file(&data, BlobSource::Own)).unwrap();
    let att = Attachment { object: w.manifest, key: w.key, media_type: "application/octet-stream".into(), size: w.size };
    let obj = create(Content::post("with a file").attach(att), Author::Signed(&a.keypair)).unwrap();
    a.node.publish(&obj).unwrap();
    b.expect_object(obj.id()).await;

    b.node.fetch_attachments(&b.node.with_store(|s| s.get(&obj.id()).unwrap().unwrap())).unwrap();
    for i in 0.. {
        match b.node.with_store(|s| s.attachment_state(&w.manifest)).unwrap() {
            Some(AttachmentState::Complete) => break,
            Some(AttachmentState::Failed(reason)) => panic!("failed: {reason}"),
            state => assert!(i < 600, "attachment never arrived: {state:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut out = Vec::new();
    let att = att_of(&b, obj.id());
    b.node.with_store(|s| s.write_attachment(&(&att).into(), &mut out)).unwrap();
    assert_eq!(out, data);
}

fn att_of(p: &Phone, post: Digest) -> Attachment {
    p.node.with_store(|s| s.get(&post).unwrap().unwrap()).object().attachments[0].clone()
}

