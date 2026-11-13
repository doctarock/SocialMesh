//! Bluetooth LE: finding nearby nodes and linking to them (SPEC.md §9).
//!
//! The Bluetooth API is different on every platform (Android, iOS, Windows,
//! BlueZ), and on phones it has to be called from the app's own language. So
//! this crate never touches the hardware. A **host driver** does the radio
//! work and talks to the crate through a small message interface:
//!
//! ```text
//!  node ── BleTransport / BleDiscovery ── BleRadio ══ Command ══▶ host driver ── radio
//!                                                 ◀══  Event  ══
//! ```
//!
//! The host advertises, scans, opens and accepts links (an L2CAP
//! connection-oriented channel, or the GATT pipe where L2CAP isn't
//! available) and moves bytes. Everything else is here: the advert format,
//! when to connect, turning links into byte streams, and address rotation.
//! [`sim::SimAir`] is a host that simulates the air between devices, for
//! tests and demos.
//!
//! Like every medium, links carry nothing but the node's encrypted channel;
//! the radio sees only ciphertext.

use mesh_discovery::{Beacon, Discovery, Medium, Sighting};
use mesh_transport::{Accepted, BoxFuture, Listener, Stream, Transport};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

pub mod codec;
pub mod sim;

/// The SocialMesh GATT service, advertised by every node that accepts links.
pub const SERVICE_UUID: &str = "191e3b36-5476-45bf-82eb-a2c4070948e7";
/// GATT pipe: the central writes (without response) here.
pub const PIPE_RX_UUID: &str = "191e3b37-5476-45bf-82eb-a2c4070948e7";
/// GATT pipe: the peripheral notifies here.
pub const PIPE_TX_UUID: &str = "191e3b38-5476-45bf-82eb-a2c4070948e7";
/// Readable copy of the advert, for platforms that can't put service data in
/// an advert (iOS).
pub const INFO_UUID: &str = "191e3b39-5476-45bf-82eb-a2c4070948e7";

/// Link ids chosen by the host (for incoming links) have this bit set; ids
/// chosen by this crate (outgoing) don't.
pub const HOST_LINK: u64 = 1 << 63;

/// A peer must have advertised this recently to be dialled.
const SEEN_FRESH: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Don't report the same device to the node more often than this.
const REPORT_EVERY: Duration = Duration::from_secs(30);
/// When both ends advertise, the end with the smaller token connects. The
/// other end connects anyway if nothing happened after this long.
const TIE_BREAK_GRACE: Duration = Duration::from_secs(20);

/// The service data in a SocialMesh advert. It says a node is here and how
/// to link to it, never who runs it.
pub mod advert {
    pub const LEN: usize = 10;
    pub const VERSION: u8 = 1;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct Advert {
        /// L2CAP channel to connect to; 0 means use the GATT pipe.
        pub psm: u16,
        /// Random, replaced whenever the radio address changes. Used to
        /// ignore our own adverts and to decide which end connects.
        pub token: [u8; 6],
    }

    /// `version | flags (0) | psm u16 | token [6]`
    pub fn encode(a: &Advert) -> Vec<u8> {
        let mut v = Vec::with_capacity(LEN);
        v.push(VERSION);
        v.push(0);
        v.extend_from_slice(&a.psm.to_be_bytes());
        v.extend_from_slice(&a.token);
        v
    }

    /// Longer data is accepted (later versions may add fields at the end).
    pub fn decode(d: &[u8]) -> Option<Advert> {
        if d.len() < LEN || d[0] != VERSION {
            return None;
        }
        Some(Advert { psm: u16::from_be_bytes([d[2], d[3]]), token: d[4..10].try_into().ok()? })
    }
}
use advert::Advert;

/// Requests from this crate to the host driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Accept links: open an L2CAP server and/or the GATT service, then
    /// answer [`Event::Listening`].
    Listen,
    /// Advertise [`SERVICE_UUID`], connectable, with this service data
    /// (replacing any previous advert). A platform that can't carry service
    /// data advertises the UUID alone and serves the data at [`INFO_UUID`].
    Advertise { service_data: Vec<u8> },
    StopAdvertising,
    /// Scan for [`SERVICE_UUID`] and report each advert as [`Event::Advert`].
    Scan,
    StopScan,
    /// Open a link to `device` (L2CAP on `psm`, or the GATT pipe if `psm` is
    /// 0). Answer [`Event::Connected`] or [`Event::ConnectFailed`].
    Connect { link: u64, device: String, psm: u16 },
    /// Bytes for a link, in order, at most the link's `mtu` long.
    Send { link: u64, data: Vec<u8> },
    /// Close a link. Unknown or already closed links are ignored.
    Close { link: u64 },
    /// Start using a new random hardware address now. Existing links may
    /// stay up (the node closes them itself).
    RotateAddress,
}

/// Reports from the host driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Accepting links: on L2CAP channel `psm`, or the GATT pipe only if 0.
    Listening { psm: u16 },
    /// A SocialMesh advert was heard. `service_data` is empty if the advert
    /// had none (read [`INFO_UUID`] after connecting if it matters).
    Advert { device: String, service_data: Vec<u8>, rssi: Option<i16> },
    /// Someone linked to us. `link` must have [`HOST_LINK`] set.
    Incoming { link: u64, device: String, mtu: usize },
    Connected { link: u64, mtu: usize },
    ConnectFailed { link: u64, reason: String },
    /// Bytes received on a link, in order.
    Data { link: u64, data: Vec<u8> },
    /// The link is gone (either end closed it, or the device left range).
    Closed { link: u64 },
}

/// The host driver's side: read commands, send events. Dropping the event
/// sender shuts the radio down.
pub struct Host {
    pub commands: mpsc::Receiver<Command>,
    pub events: mpsc::Sender<Event>,
}

/// The radio as the rest of the crate sees it. Cheap to clone.
#[derive(Clone)]
pub struct BleRadio(Arc<RadioInner>);

struct RadioInner {
    cmd: mpsc::Sender<Command>,
    next_link: AtomicU64,
    state: Mutex<RadioState>,
    adverts: broadcast::Sender<(String, Option<Advert>)>,
    /// Some(psm) once the host is accepting links.
    listening: watch::Sender<Option<u16>>,
    /// Bumped on every address rotation.
    epoch: watch::Sender<u64>,
}

#[derive(Default)]
struct RadioState {
    links: HashMap<u64, (String, mpsc::Sender<Vec<u8>>)>,
    connecting: HashMap<u64, (String, oneshot::Sender<Result<Stream, String>>)>,
    incoming: Option<mpsc::Sender<Accepted>>,
    /// device → (last advert, when heard)
    seen: HashMap<String, (Option<Advert>, Instant)>,
}

impl BleRadio {
    /// A radio and the host end its driver uses. Call inside a tokio runtime.
    pub fn new() -> (BleRadio, Host) {
        let (cmd, commands) = mpsc::channel(256);
        let (events, mut ev_rx) = mpsc::channel(256);
        let inner = Arc::new(RadioInner {
            cmd,
            next_link: AtomicU64::new(1),
            state: Mutex::new(RadioState::default()),
            adverts: broadcast::channel(256).0,
            listening: watch::channel(None).0,
            epoch: watch::channel(0).0,
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                handle_event(&inner, ev).await;
            }
        });
        (BleRadio(inner), Host { commands, events })
    }

    async fn command(&self, c: Command) -> io::Result<()> {
        self.0.cmd.send(c).await.map_err(|_| io::Error::new(io::ErrorKind::NotConnected, "Bluetooth host stopped"))
    }

    /// The advert last heard from `device`, if it was heard recently.
    pub fn seen(&self, device: &str) -> Option<Option<Advert>> {
        let st = self.0.state.lock().unwrap();
        st.seen.get(device).filter(|(_, at)| at.elapsed() < SEEN_FRESH).map(|(a, _)| *a)
    }

    /// Devices we have an open link with.
    pub fn linked_devices(&self) -> HashSet<String> {
        self.0.state.lock().unwrap().links.values().map(|(d, _)| d.clone()).collect()
    }

    /// Open a link to a device that has advertised recently.
    pub async fn connect(&self, device: &str) -> io::Result<Stream> {
        let Some(advert) = self.seen(device) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "device not in range"));
        };
        let psm = advert.map(|a| a.psm).unwrap_or(0);
        let link = self.0.next_link.fetch_add(1, Ordering::Relaxed) & !HOST_LINK;
        let (tx, rx) = oneshot::channel();
        self.0.state.lock().unwrap().connecting.insert(link, (device.to_string(), tx));
        self.command(Command::Connect { link, device: device.to_string(), psm }).await?;
        match tokio::time::timeout(CONNECT_TIMEOUT, rx).await {
            Ok(Ok(Ok(stream))) => Ok(stream),
            Ok(Ok(Err(reason))) => Err(io::Error::new(io::ErrorKind::ConnectionRefused, reason)),
            Ok(Err(_)) => Err(io::Error::new(io::ErrorKind::NotConnected, "Bluetooth host stopped")),
            Err(_) => {
                self.0.state.lock().unwrap().connecting.remove(&link);
                let _ = self.command(Command::Close { link }).await;
                Err(io::Error::new(io::ErrorKind::TimedOut, "Bluetooth connect timed out"))
            }
        }
    }

    /// Start accepting links. Only one listener at a time.
    pub async fn listen(&self) -> io::Result<mpsc::Receiver<Accepted>> {
        let (tx, rx) = mpsc::channel(16);
        self.0.state.lock().unwrap().incoming = Some(tx);
        self.command(Command::Listen).await?;
        Ok(rx)
    }

    /// New hardware address and, through [`BleDiscovery`], a new advert token.
    pub fn rotate_address(&self) {
        let cmd = self.0.cmd.clone();
        tokio::spawn(async move {
            let _ = cmd.send(Command::RotateAddress).await;
        });
        self.0.epoch.send_modify(|e| *e += 1);
    }
}

async fn handle_event(inner: &Arc<RadioInner>, ev: Event) {
    match ev {
        Event::Listening { psm } => {
            inner.listening.send_replace(Some(psm));
        }
        Event::Advert { device, service_data, rssi: _ } => {
            let advert = if service_data.is_empty() {
                None
            } else {
                match advert::decode(&service_data) {
                    Some(a) => Some(a),
                    None => return, // a version we don't speak
                }
            };
            {
                let mut st = inner.state.lock().unwrap();
                if st.seen.len() >= 1024 && !st.seen.contains_key(&device) {
                    st.seen.retain(|_, (_, at)| at.elapsed() < SEEN_FRESH);
                    if st.seen.len() >= 1024 {
                        return; // bounded, even in a crowd
                    }
                }
                st.seen.insert(device.clone(), (advert, Instant::now()));
            }
            let _ = inner.adverts.send((device, advert));
        }
        Event::Incoming { link, device, mtu } => {
            let incoming = inner.state.lock().unwrap().incoming.clone();
            let accepted = match incoming {
                Some(tx) if link & HOST_LINK != 0 => {
                    let stream = open_link(inner, link, device.clone(), mtu);
                    tx.try_send(Accepted { stream, remote: device }).is_ok()
                }
                _ => false,
            };
            if !accepted {
                inner.state.lock().unwrap().links.remove(&link);
                let _ = inner.cmd.send(Command::Close { link }).await;
            }
        }
        Event::Connected { link, mtu } => {
            let pending = inner.state.lock().unwrap().connecting.remove(&link);
            match pending {
                Some((device, reply)) => {
                    let stream = open_link(inner, link, device, mtu);
                    if reply.send(Ok(stream)).is_err() {
                        inner.state.lock().unwrap().links.remove(&link); // gave up waiting
                    }
                }
                None => {
                    let _ = inner.cmd.send(Command::Close { link }).await;
                }
            }
        }
        Event::ConnectFailed { link, reason } => {
            if let Some((_, reply)) = inner.state.lock().unwrap().connecting.remove(&link) {
                let _ = reply.send(Err(reason));
            }
        }
        Event::Data { link, data } => {
            let tx = inner.state.lock().unwrap().links.get(&link).map(|(_, tx)| tx.clone());
            if let Some(tx) = tx {
                let _ = tx.send(data).await;
            }
        }
        Event::Closed { link } => {
            let mut st = inner.state.lock().unwrap();
            st.links.remove(&link);
            if let Some((_, reply)) = st.connecting.remove(&link) {
                let _ = reply.send(Err("link closed".into()));
            }
        }
    }
}

/// Turn a host link into a byte stream for the node.
fn open_link(inner: &Arc<RadioInner>, link: u64, device: String, mtu: usize) -> Stream {
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(256);
    inner.state.lock().unwrap().links.insert(link, (device, data_tx));
    let (mut rd, mut wr) = tokio::io::split(ours);
    // Radio → node.
    tokio::spawn(async move {
        while let Some(d) = data_rx.recv().await {
            if wr.write_all(&d).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    // Node → radio, cut to the link's MTU.
    let cmd = inner.cmd.clone();
    let weak: Weak<RadioInner> = Arc::downgrade(inner);
    let mtu = mtu.clamp(20, 64 * 1024);
    tokio::spawn(async move {
        let mut buf = vec![0u8; mtu];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if cmd.send(Command::Send { link, data: buf[..n].to_vec() }).await.is_err() {
                        break;
                    }
                }
            }
        }
        if let Some(inner) = weak.upgrade() {
            inner.state.lock().unwrap().links.remove(&link);
        }
        let _ = cmd.send(Command::Close { link }).await;
    });
    Box::new(theirs)
}

/// Bluetooth LE as a node transport. Addresses are device addresses as the
/// host reports them; the node writes them `ble:<device>`.
#[derive(Clone)]
pub struct BleTransport(pub BleRadio);

impl Transport for BleTransport {
    fn name(&self) -> &'static str {
        "ble"
    }

    fn dial<'a>(&'a self, addr: &'a str) -> BoxFuture<'a, io::Result<Stream>> {
        Box::pin(self.0.connect(addr))
    }

    /// The address is ignored: there is one radio.
    fn listen<'a>(&'a self, _addr: &'a str) -> BoxFuture<'a, io::Result<Box<dyn Listener>>> {
        Box::pin(async move {
            let rx = self.0.listen().await?;
            Ok(Box::new(BleListener(rx)) as Box<dyn Listener>)
        })
    }

    fn rotate_address(&self) {
        self.0.rotate_address();
    }
}

struct BleListener(mpsc::Receiver<Accepted>);

impl Listener for BleListener {
    fn local_addr(&self) -> String {
        "radio".into()
    }

    fn accept(&mut self) -> BoxFuture<'_, io::Result<Accepted>> {
        Box::pin(async move {
            self.0.recv().await.ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Bluetooth host stopped"))
        })
    }
}

/// Bluetooth LE adverts as a discovery medium. Advertises (unless the node
/// is listen-only) once the transport is accepting links, scans, and reports
/// nearby nodes as `ble:<device>`.
pub struct BleDiscovery(pub BleRadio);

impl Discovery for BleDiscovery {
    fn medium(&self) -> Medium {
        Medium::Bluetooth
    }

    fn run(self: Arc<Self>, beacon: Option<Beacon>, out: mpsc::Sender<Sighting>) -> BoxFuture<'static, io::Result<()>> {
        Box::pin(async move {
            let radio = &self.0;
            let mut adverts = radio.0.adverts.subscribe();
            let mut listening = radio.0.listening.subscribe();
            let mut epoch = radio.0.epoch.subscribe();
            radio.command(Command::Scan).await?;

            let announce = beacon.is_some();
            let mut token: [u8; 6] = rand::random();
            let mut advertised: Option<Advert> = None;
            // device → first heard, last reported
            let mut heard: HashMap<String, (Instant, Option<Instant>)> = HashMap::new();
            loop {
                if announce {
                    let current = (*listening.borrow_and_update()).map(|psm| Advert { psm, token });
                    if current != advertised {
                        if let Some(a) = current {
                            radio.command(Command::Advertise { service_data: advert::encode(&a) }).await?;
                        }
                        advertised = current;
                    }
                }
                tokio::select! {
                    r = listening.changed() => if r.is_err() { return Ok(()) },
                    r = epoch.changed() => {
                        if r.is_err() {
                            return Ok(());
                        }
                        epoch.borrow_and_update();
                        token = rand::random();
                        heard.clear(); // our new self has heard nobody
                    }
                    r = adverts.recv() => {
                        let (device, theirs) = match r {
                            Ok(v) => v,
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        };
                        if theirs.is_some_and(|a| a.token == token) || radio.linked_devices().contains(&device) {
                            continue;
                        }
                        let now = Instant::now();
                        if heard.len() > 4096 {
                            heard.clear();
                        }
                        let (first, last) = heard.entry(device.clone()).or_insert((now, None));
                        if last.is_some_and(|t| now.duration_since(t) < REPORT_EVERY) {
                            continue;
                        }
                        // Both ends hear each other; one connecting is enough.
                        let ours_to_dial = !announce || theirs.map_or(true, |a| token < a.token);
                        if !ours_to_dial && now.duration_since(*first) < TIE_BREAK_GRACE {
                            continue;
                        }
                        *last = Some(now);
                        let s = Sighting { medium: Medium::Bluetooth, addr: format!("ble:{device}"), identity: None };
                        if out.send(s).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advert_roundtrip_and_junk() {
        let a = Advert { psm: 0x0081, token: [1, 2, 3, 4, 5, 6] };
        let e = advert::encode(&a);
        assert_eq!(e.len(), advert::LEN);
        assert_eq!(advert::decode(&e), Some(a));
        let mut longer = e.clone();
        longer.extend_from_slice(b"future");
        assert_eq!(advert::decode(&longer), Some(a));
        assert_eq!(advert::decode(&e[..9]), None);
        let mut v2 = e;
        v2[0] = 2;
        assert_eq!(advert::decode(&v2), None);
    }

    #[test]
    fn advert_fits_a_legacy_advertising_packet() {
        // flags AD (3) + 128-bit service data AD (2 + 16 + data) <= 31
        const { assert!(3 + 2 + 16 + advert::LEN <= 31) };
    }
}
