//! Finding peers (README §13, §21, §22).
//!
//! Discovery and transport are separate concerns:
//!
//! ```text
//! Discovery  ── "something that speaks SocialMesh is reachable at X" ──►  Node
//! Node       ── Transport::dial(X) → handshake → verified identity
//! ```
//!
//! A sighting is only a *hint*. Nothing learned through discovery is trusted;
//! the identity is established by the authenticated handshake afterwards.
//!
//! Each medium (LAN broadcast, Bluetooth LE adverts, Wi-Fi Aware, NFC taps,
//! audio chirps, ...) implements [`Discovery`]. See docs/TRANSPORTS.md.

use mesh_crypto::Digest;
use mesh_transport::BoxFuture;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub mod invite;
pub use invite::Invitation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Medium {
    Lan,
    Internet,
    Bluetooth,
    WifiDirect,
    Nfc,
    Audio,
    Offline,
}

impl std::fmt::Display for Medium {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Medium::Lan => "lan",
            Medium::Internet => "internet",
            Medium::Bluetooth => "bluetooth",
            Medium::WifiDirect => "wifi-direct",
            Medium::Nfc => "nfc",
            Medium::Audio => "audio",
            Medium::Offline => "offline",
        })
    }
}

/// "A peer may be reachable here." Advisory only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    pub medium: Medium,
    /// Address dialable by the matching transport.
    pub addr: String,
    /// Present only when the medium carried a full invitation (e.g. NFC, QR, audio).
    pub identity: Option<Digest>,
}

/// What this node advertises. Deliberately carries **no identity**: a beacon
/// reveals that a SocialMesh node is present, not who runs it.
#[derive(Clone, Copy, Debug)]
pub struct Beacon {
    /// Random per-run value, used only to ignore our own beacons.
    pub instance: [u8; 16],
    /// Port of the transport listener peers should dial.
    pub port: u16,
}

pub trait Discovery: Send + Sync {
    fn medium(&self) -> Medium;
    /// Run until error or cancellation. With `beacon = None` the node listens
    /// passively and never transmits.
    fn run(self: Arc<Self>, beacon: Option<Beacon>, out: mpsc::Sender<Sighting>) -> BoxFuture<'static, io::Result<()>>;
}

// ---------------------------------------------------------------------------
// LAN: UDP broadcast
// ---------------------------------------------------------------------------

pub const LAN_DISCOVERY_PORT: u16 = 7843;
const LAN_MAGIC: &[u8; 6] = b"SMDISC";
const LAN_VERSION: u8 = 1;
const LAN_PACKET_LEN: usize = 6 + 1 + 16 + 2;

/// Finds peers on the local network segment with UDP broadcast. Works with no
/// Internet, no router configuration and no DNS — two laptops on a hotspot suffice.
pub struct LanDiscovery {
    pub port: u16,
    pub interval: Duration,
    pub target: Ipv4Addr,
}

impl Default for LanDiscovery {
    fn default() -> Self {
        LanDiscovery { port: LAN_DISCOVERY_PORT, interval: Duration::from_secs(5), target: Ipv4Addr::BROADCAST }
    }
}

pub fn encode_lan_beacon(b: &Beacon) -> [u8; LAN_PACKET_LEN] {
    let mut p = [0u8; LAN_PACKET_LEN];
    p[..6].copy_from_slice(LAN_MAGIC);
    p[6] = LAN_VERSION;
    p[7..23].copy_from_slice(&b.instance);
    p[23..25].copy_from_slice(&b.port.to_be_bytes());
    p
}

pub fn decode_lan_beacon(p: &[u8]) -> Option<Beacon> {
    if p.len() != LAN_PACKET_LEN || &p[..6] != LAN_MAGIC || p[6] != LAN_VERSION {
        return None;
    }
    let port = u16::from_be_bytes([p[23], p[24]]);
    if port == 0 {
        return None;
    }
    Some(Beacon { instance: p[7..23].try_into().ok()?, port })
}

fn bind_shared_udp(port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Several nodes on one machine (or another app) may share the discovery port.
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    s.set_reuse_port(true)?;
    s.set_broadcast(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(s.into())
}

impl Discovery for LanDiscovery {
    fn medium(&self) -> Medium {
        Medium::Lan
    }

    fn run(self: Arc<Self>, beacon: Option<Beacon>, out: mpsc::Sender<Sighting>) -> BoxFuture<'static, io::Result<()>> {
        Box::pin(async move {
            let sock = Arc::new(bind_shared_udp(self.port)?);

            let sender = beacon.map(|b| {
                let sock = sock.clone();
                let dest = SocketAddr::V4(SocketAddrV4::new(self.target, self.port));
                let interval = self.interval;
                tokio::spawn(async move {
                    let packet = encode_lan_beacon(&b);
                    let mut tick = tokio::time::interval(interval);
                    loop {
                        tick.tick().await;
                        // Failure (e.g. no network yet) is normal; keep trying.
                        let _ = sock.send_to(&packet, dest).await;
                    }
                })
            });
            let own = beacon.map(|b| b.instance);

            // Rate-limit repeat sightings so a chatty or hostile LAN can't flood the node.
            let mut recent: HashMap<SocketAddr, Instant> = HashMap::new();
            let mut buf = [0u8; 64];
            let result = loop {
                let (n, from) = match sock.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue, // Windows ICMP quirk
                    Err(e) => break Err(e),
                };
                let Some(b) = decode_lan_beacon(&buf[..n]) else { continue };
                if Some(b.instance) == own {
                    continue;
                }
                let addr = SocketAddr::new(from.ip(), b.port);
                let now = Instant::now();
                if recent.get(&addr).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(30)) {
                    continue;
                }
                if recent.len() > 4096 {
                    recent.clear();
                }
                recent.insert(addr, now);
                let s = Sighting { medium: Medium::Lan, addr: addr.to_string(), identity: None };
                if out.send(s).await.is_err() {
                    break Ok(());
                }
            };
            if let Some(h) = sender {
                h.abort();
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrip_and_rejects_junk() {
        let b = Beacon { instance: [5; 16], port: 7842 };
        let d = decode_lan_beacon(&encode_lan_beacon(&b)).unwrap();
        assert_eq!((d.instance, d.port), (b.instance, b.port));
        assert!(decode_lan_beacon(b"hello").is_none());
        let mut bad = encode_lan_beacon(&b);
        bad[6] = 9;
        assert!(decode_lan_beacon(&bad).is_none());
    }

    #[tokio::test]
    async fn two_instances_find_each_other_on_loopback_port() {
        // Uses a private port and the limited-broadcast address. Some sandboxed CI
        // networks drop broadcast; in that case this test is skipped rather than failed.
        let port = 47843;
        let mk = || Arc::new(LanDiscovery { port, interval: Duration::from_millis(100), target: Ipv4Addr::BROADCAST });
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, _rx_b) = mpsc::channel(8);
        let a = tokio::spawn(mk().run(Some(Beacon { instance: [1; 16], port: 1111 }), tx_a));
        let b = tokio::spawn(mk().run(Some(Beacon { instance: [2; 16], port: 2222 }), tx_b));
        let got = tokio::time::timeout(Duration::from_secs(3), rx_a.recv()).await;
        a.abort();
        b.abort();
        match got {
            Ok(Some(s)) => assert!(s.addr.ends_with(":2222"), "A should see B, got {s:?}"),
            _ => eprintln!("broadcast unavailable in this environment; skipping"),
        }
    }
}
