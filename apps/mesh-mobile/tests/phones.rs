//! Two phone nodes whose "Bluetooth driver" is a pair of threads moving
//! encoded commands and events between the library and the simulated air,
//! the way the Android driver does with the real radio.

use mesh_transport_ble::codec::{decode_command, encode_event};
use mesh_transport_ble::sim::{SimAir, SimConfig, SimDevice};
use mesh_transport_ble::Host;
use socialmesh::{Mobile, Options};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// What the app's driver does: pull commands, push events, nothing else.
fn driver(m: Arc<Mobile>, air: &SimAir, rt: &tokio::runtime::Runtime, x: f64) -> SimDevice {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    let device = {
        let _g = rt.enter();
        air.attach(Host { commands: cmd_rx, events: ev_tx }, x, 0.0)
    };
    let m2 = m.clone();
    std::thread::spawn(move || {
        while let Some(bytes) = m2.next_radio_command(Duration::from_millis(100)).or(Some(vec![])) {
            if bytes.is_empty() {
                continue;
            }
            let c = decode_command(&bytes).expect("library sends valid commands");
            if cmd_tx.blocking_send(c).is_err() {
                return;
            }
        }
    });
    std::thread::spawn(move || {
        while let Some(ev) = ev_rx.blocking_recv() {
            assert!(m.radio_event(&encode_event(&ev)), "library refused a valid event");
        }
    });
    device
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn two_phones_sync_over_the_encoded_driver_interface() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let air = {
        let _g = rt.enter();
        SimAir::new(SimConfig::default())
    };
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let opts = Options { beacon: true, lan: false };
    let a = Arc::new(Mobile::start(da.path(), opts).unwrap());
    let b = Arc::new(Mobile::start(db.path(), opts).unwrap());
    let _da = driver(a.clone(), &air, &rt, 0.0);
    let _db = driver(b.clone(), &air, &rt, 10.0);

    wait_for("a Bluetooth link", || a.links().contains(" ble") && b.links().contains(" ble"));
    let id = a.publish("hello from a phone \"quoted\"\nsecond line").unwrap();
    wait_for("the post", || b.feed_json(10).unwrap().contains(&id));

    let feed = b.feed_json(10).unwrap();
    assert!(feed.contains(r#""mine":false"#), "{feed}");
    assert!(feed.contains(r#""body":"hello from a phone \"quoted\"\nsecond line""#), "{feed}");
    assert!(a.feed_json(10).unwrap().contains(r#""mine":true"#));

    // The UI's event stream says what happened.
    let mut lines = Vec::new();
    while let Some(l) = b.next_event(Duration::from_millis(100)) {
        lines.push(l);
    }
    assert!(lines.iter().any(|l| l.starts_with("Connected") && l.ends_with("over ble")), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("New post")), "{lines:?}");
}

#[test]
fn identity_survives_a_restart_and_matches_the_cli_format() {
    let dir = tempfile::tempdir().unwrap();
    let opts = Options { beacon: false, lan: false };
    let first = Mobile::start(dir.path(), opts).unwrap();
    let (me, link1) = first.ids();
    drop(first);
    let second = Mobile::start(dir.path(), opts).unwrap();
    let (me2, link2) = second.ids();
    assert_eq!(me, me2, "posting identity is stable");
    assert_ne!(link1, link2, "link key is fresh every run");
    let key = std::fs::read_to_string(dir.path().join("identity.key")).unwrap();
    assert_eq!(key.trim().len(), 64, "hex seed, like `mesh init`");
}
