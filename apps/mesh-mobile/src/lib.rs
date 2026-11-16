//! A SocialMesh node inside a phone app.
//!
//! The app does two things: it runs the Bluetooth driver (moving bytes
//! between this library and the platform's Bluetooth API, see
//! `mesh_transport_ble::codec`), and it shows posts. Everything else, from
//! syncing to encryption to address rotation, happens in here.
//!
//! [`Mobile`] is the plain Rust API; [`jni`](mod@jni) wraps it for Android
//! (`org.socialmesh.android.Native`). Every call is blocking and safe from
//! any thread.

use anyhow::{Context, Result};
use mesh_crypto::{Digest, Keypair};
use mesh_discovery::LanDiscovery;
use mesh_node::{Config, Event as NodeEvent, Node, Running};
use mesh_object::{create, Author, Content, VerifiedObject};
use mesh_store::Store;
use mesh_transport_ble::{codec, BleDiscovery, BleRadio, BleTransport, Command, Event as BleEvent};
use mesh_transport_tcp::{TcpTransport, DEFAULT_PORT};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

pub mod jni;
mod sound;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Advertise over Bluetooth (and the LAN). Off: listen only, invisible
    /// until we connect to someone.
    pub beacon: bool,
    /// Also use Wi-Fi / LAN (TCP plus LAN discovery).
    pub lan: bool,
}

pub struct Mobile {
    // Field order matters: the node and radio go before the runtime.
    node: Node,
    running: Mutex<Option<Running>>,
    commands: Mutex<mpsc::Receiver<Command>>,
    radio_events: mpsc::Sender<BleEvent>,
    node_events: Mutex<broadcast::Receiver<NodeEvent>>,
    me: Keypair,
    ears: Mutex<sound::Ears>,
    rt: tokio::runtime::Runtime,
}

impl Mobile {
    /// Open (or create) the node in `home` and start it.
    pub fn start(home: &Path, opts: Options) -> Result<Mobile> {
        std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
        let me = load_or_create_identity(home)?;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let _enter = rt.enter();

        let (radio, host) = BleRadio::new();
        let config = Config {
            listen: opts.lan.then(|| format!("0.0.0.0:{DEFAULT_PORT}")),
            beacon: opts.beacon,
            // A phone: new link key (and Bluetooth address) every run and daily.
            link_key_rotation: Some(Duration::from_secs(24 * 3600)),
            // Phones are on batteries and slow radios: less decoy traffic.
            cover_bytes_per_hour: 2 * 1024 * 1024,
            cache_budget: 512 * 1024 * 1024,
            ..Config::default()
        };
        let store = Store::open(home)?;
        let node = Node::new(Keypair::generate(), store, Arc::new(TcpTransport), config);
        node.add_transport(Arc::new(BleTransport(radio.clone())), Some("radio".into()));
        node.add_discovery(Arc::new(BleDiscovery(radio)));
        if opts.lan {
            node.add_discovery(Arc::new(LanDiscovery::default()));
        }
        let node_events = node.subscribe();
        let running = rt.block_on(node.start())?;
        Ok(Mobile {
            node,
            running: Mutex::new(Some(running)),
            commands: Mutex::new(host.commands),
            radio_events: host.events,
            node_events: Mutex::new(node_events),
            me,
            ears: Mutex::new(sound::Ears::default()),
            rt,
        })
    }

    /// Stop the node. Further calls do nothing useful.
    pub fn stop(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            self.rt.block_on(r.shutdown());
        }
    }

    /// The next command for the Bluetooth driver, encoded
    /// (`mesh_transport_ble::codec`), or `None` after `timeout`. The driver
    /// calls this in a loop on its own thread.
    pub fn next_radio_command(&self, timeout: Duration) -> Option<Vec<u8>> {
        let mut rx = self.commands.lock().unwrap();
        self.rt
            .block_on(async { tokio::time::timeout(timeout, rx.recv()).await.ok().flatten() })
            .map(|c| codec::encode_command(&c))
    }

    /// An encoded event from the Bluetooth driver. Blocks while the node is
    /// catching up (that is the flow control). `false` if it couldn't be
    /// decoded or the node has stopped.
    pub fn radio_event(&self, encoded: &[u8]) -> bool {
        match codec::decode_event(encoded) {
            Some(ev) => self.radio_events.blocking_send(ev).is_ok(),
            None => false,
        }
    }

    /// Publish a signed text post, announced from this device. Returns its id.
    pub fn publish(&self, text: &str) -> Result<String> {
        let obj = create(Content::post(text), Author::Signed(&self.me))?;
        self.node.publish(&obj)?;
        Ok(obj.id().to_hex())
    }

    /// Newest top-level posts as a JSON array of
    /// `{"id","author","mine","created_ms","body"}`.
    pub fn feed_json(&self, limit: usize) -> Result<String> {
        let me = self.me.public().id();
        let roots = self.node.with_store(|s| s.roots(limit))?;
        let mut out = String::from("[");
        for (i, o) in roots.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_post(&mut out, o, me);
        }
        out.push(']');
        Ok(out)
    }

    /// The next thing worth telling the user, as a line of text, or `None`
    /// after `timeout`.
    pub fn next_event(&self, timeout: Duration) -> Option<String> {
        let mut rx = self.node_events.lock().unwrap();
        self.rt.block_on(async {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) => return None,
                    Ok(Err(broadcast::error::RecvError::Lagged(n))) => return Some(format!("({n} events skipped)")),
                    Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                    Ok(Ok(ev)) => {
                        if let Some(line) = describe(&ev) {
                            return Some(line);
                        }
                    }
                }
            }
        })
    }

    /// Live links, one `peer medium` pair per line.
    pub fn links(&self) -> String {
        self.node.links().iter().map(|(p, m)| format!("{} {m}\n", p.short())).collect()
    }

    /// The posting identity (stable) and current link key (rotates).
    pub fn ids(&self) -> (String, String) {
        (self.me.public().id().short(), self.node.identity().id().short())
    }
}

impl Drop for Mobile {
    fn drop(&mut self) {
        self.stop();
    }
}

fn describe(ev: &NodeEvent) -> Option<String> {
    Some(match ev {
        NodeEvent::PeerConnected { peer, medium, .. } => format!("Connected {} over {medium}", peer.short()),
        NodeEvent::PeerDisconnected { peer, medium, .. } => format!("Disconnected {} over {medium}", peer.short()),
        NodeEvent::ObjectReceived { object, .. } => format!("New post {}", object.id().short()),
        NodeEvent::Discovered { medium, .. } => format!("Found a node via {medium}"),
        NodeEvent::LinkKeyRotated { id } => format!("New link key {}", id.short()),
        NodeEvent::Listening(a) => format!("Listening on {a}"),
        _ => return None,
    })
}

fn push_post(out: &mut String, o: &VerifiedObject, me: Digest) {
    let obj = o.object();
    let author = obj.author.as_ref().map(|a| a.id());
    let _ = write!(out, "{{\"id\":\"{}\",\"author\":", o.id().to_hex());
    match author {
        Some(a) => {
            let _ = write!(out, "\"{}\"", a.short());
        }
        None => out.push_str("null"),
    }
    let _ = write!(out, ",\"mine\":{},\"created_ms\":{},\"body\":", author == Some(me), obj.created_ms);
    push_json_str(out, &obj.body);
    out.push('}');
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Same file and format as the CLI (`identity.key`, hex seed), so a home
/// directory works with both.
fn load_or_create_identity(home: &Path) -> Result<Keypair> {
    let path = home.join("identity.key");
    if !path.exists() {
        std::fs::write(&path, hex::encode(mesh_crypto::random_bytes::<32>()))?;
    }
    let raw = hex::decode(std::fs::read_to_string(&path)?.trim()).context("identity.key is not hex")?;
    let seed: [u8; 32] = raw.try_into().map_err(|_| anyhow::anyhow!("identity.key has the wrong length"))?;
    Ok(Keypair::from_seed(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_are_escaped() {
        let mut s = String::new();
        push_json_str(&mut s, "a\"b\\c\nd\u{1}e\u{2028}é");
        assert_eq!(s, concat!(r#""a\"b\\c\nd\u0001e"#, r"\u2028", "\u{e9}\""));
    }
}
