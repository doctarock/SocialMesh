//! Node runtime: accepts and dials peers, authenticates them, and syncs objects.
//!
//! Sync is pull-based (SPEC.md §5.3):
//!
//! ```text
//! A ── HAVE [ids] ──► B        "I can supply these"
//! A ◄── WANT [ids] ── B        B asks only for what it lacks, within its budget
//! A ── OBJECT bytes ─► B       B verifies hash + signature, then decides to retain
//! ```
//!
//! Each stage of README §12 is a separate decision here:
//! receive → validate (`VerifiedObject::decode`) → retain (`Store::put`) →
//! propagate (`Config::relay` + origin) → display (the application's job).
//!
//! Anonymous objects, and signed objects whose author doesn't want this
//! device known as theirs, take a different first leg (docs/ORIGIN-PRIVACY.md):
//!
//! ```text
//! author ── STEM ─► relay ── STEM ─► relay ──(coin flip)── fluff: HAVE to everyone
//! ```
//!
//! During the stem phase the object is embargoed: no node offers or serves it.
//! Every hop, including the author's own, handles it identically, so the node
//! that first makes it public is usually not the author.
//!
//! Attachments are fetched separately, on demand, chunk by chunk from any
//! peers that have them (see [`fetch`]).
//!
//! A peer can be connected over several media at once (TCP, Bluetooth, Wi-Fi
//! Direct, ...), one link per medium. Each link is a full, separately
//! encrypted session. Traffic to a peer is spread across all of its links:
//! attachment requests go to whichever link is least loaded, stem pieces are
//! shared out with each link paced on its own, and control messages pick a
//! random link. An observer of any one medium sees only part of it.
//!
//! The key a node proves on its links is a **link key**, not the user's posting
//! identity. Anyone in radio range can connect and learn the link key, so it
//! must not say who writes what. It can rotate ([`Node::rotate_link_key`]);
//! relays and nodes reached by invitation keep a stable one.

mod fetch;

use anyhow::{bail, Context, Result};
use mesh_crypto::{random_bytes, Digest, Keypair, PublicKey};
use mesh_discovery::{Beacon, Discovery, Medium, Sighting};
use mesh_object::VerifiedObject;
use mesh_protocol::secure::{secure, Role};
use mesh_protocol::{
    hello_input, read_message, write_message, Message, FLAG_ACCEPTS_STEM, MAX_IDS_PER_MESSAGE, PROTOCOL_VERSION,
};
use rand::Rng;
use mesh_object::blob::BLOB_SIZE;
use mesh_store::{BlobSource, Origin, Store};
use mesh_transport::{Listener, Stream, Transport};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio::task::JoinSet;

#[derive(Clone, Debug)]
pub struct Config {
    /// Address to accept inbound connections on. `None` = outbound only.
    pub listen: Option<String>,
    /// Offer objects received from others onward (README §14 "relay permission").
    /// Objects authored locally are always offered.
    pub relay: bool,
    /// How often to scan for peers needing a connection. Failing addresses
    /// back off exponentially from this up to one minute.
    pub dial_interval: Duration,
    pub announce_interval: Duration,
    pub handshake_timeout: Duration,
    /// Outstanding WANTs per connection. Bounds memory a peer can make us commit.
    pub max_pending_wants: usize,
    /// How long to wait for a wanted object before forgetting the request.
    pub want_timeout: Duration,
    pub max_connections: usize,
    /// Invalid/unsolicited objects tolerated before disconnecting a peer.
    pub max_strikes: u32,
    /// Transmit discovery beacons. When false, discovery only listens.
    /// Beacons reveal that a node is present, never which identity runs it.
    pub beacon: bool,
    /// Chance that a relay ends the stem and makes an anonymous object public.
    /// Expected stem length is 1/p hops. The author never fluffs at hop 0.
    pub stem_fluff_probability: f64,
    /// Each stem holder fluffs anyway if it has not seen the object go public
    /// within a random time in this range (protects against dropped stems).
    pub stem_embargo: (Duration, Duration),
    /// How long a node keeps the same stem successor. A stable route stops a
    /// peer from learning the author by watching which successor changes.
    pub stem_epoch: Duration,
    /// A relay that gets a STEM before all of its file (pieces still coming
    /// over a slower link) waits for them while pieces keep arriving. It gives
    /// up after this long with no new piece, and ends the stem itself.
    pub stem_assembly_idle: Duration,
    /// STEM messages accepted per peer per minute.
    pub max_stems_per_minute: u32,
    /// Send a PAD on a link that has been quiet this long, so the other end
    /// knows it is still there.
    pub keepalive: Duration,
    /// Close a link that has sent nothing for this long (a peer that walked
    /// out of range rarely says goodbye).
    pub link_timeout: Duration,
    /// Two links over the same medium opened from opposite ends within this
    /// window are a dial race: both ends keep the same one. Otherwise the newer
    /// link replaces the older, so a peer coming back into range always
    /// reconnects even if we still hold its dead link.
    pub link_race_window: Duration,
    /// Replace the link key this often (with ±10% jitter), closing every link
    /// so no peer sees the old and new key side by side. `None` keeps it.
    pub link_key_rotation: Option<Duration>,
    /// Forget a discovered address this long after it was last sighted.
    pub discovered_ttl: Duration,
    /// STASH blobs (64 KiB each) accepted per peer per minute.
    pub max_stash_per_minute: u32,
    /// Largest total attachment size an anonymous object may carry. Every stem
    /// hop pushes the whole file to the next, so this bounds what a route costs.
    pub max_anonymous_attachments: u64,
    /// Gap between successive STASH pieces on a link, random within this range.
    /// Real sends and cover traffic are paced identically, so they look alike.
    pub stash_pace: (Duration, Duration),
    /// Cover traffic: average bytes of decoy STASH pieces sent per hour, in
    /// runs of random length at random times. 0 disables it. Without cover, a
    /// link observer can spot the run of pieces an anonymous post with a file
    /// sends from the author's device.
    pub cover_bytes_per_hour: u64,
    /// Longest decoy run, in pieces. Matches the largest real anonymous send.
    pub cover_max_run: usize,
    /// Decoy single messages per hour, sized like a post, sent at random
    /// times to relaying peers. A real STEM leaving the author's device is a
    /// message that nothing triggered; these make every node send such
    /// messages all the time. 0 disables. A few KB per hour.
    pub cover_messages_per_hour: u32,
    /// The author holds a new private-origin post for a random time in this
    /// range before sending it, so it doesn't leave the moment it is written.
    pub author_delay: (Duration, Duration),
    /// Most bytes of cached blobs to keep (cover traffic, relayed files).
    /// Blobs of our own signed posts don't count and are never evicted.
    pub cache_budget: u64,
    /// Relaying nodes fetch attachments up to this size automatically, so a
    /// file stays available after its author goes offline. 0 disables.
    /// Non-relaying nodes only fetch what the user asks for.
    pub auto_fetch_max: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: None,
            relay: true,
            dial_interval: Duration::from_secs(2),
            announce_interval: Duration::from_millis(500),
            handshake_timeout: Duration::from_secs(10),
            max_pending_wants: 4096,
            want_timeout: Duration::from_secs(60),
            max_connections: 64,
            max_strikes: 16,
            beacon: true,
            stem_fluff_probability: 0.1,
            stem_embargo: (Duration::from_secs(20), Duration::from_secs(50)),
            stem_epoch: Duration::from_secs(600),
            stem_assembly_idle: Duration::from_secs(15),
            max_stems_per_minute: 120,
            keepalive: Duration::from_secs(10),
            link_timeout: Duration::from_secs(35),
            link_race_window: Duration::from_secs(5),
            link_key_rotation: None,
            discovered_ttl: Duration::from_secs(180),
            max_stash_per_minute: 1024,
            max_anonymous_attachments: 16 * 1024 * 1024,
            stash_pace: (Duration::from_millis(20), Duration::from_millis(120)),
            cover_bytes_per_hour: 16 * 1024 * 1024,
            cover_max_run: 16 * 1024 * 1024 / BLOB_SIZE + 1,
            cover_messages_per_hour: 60,
            author_delay: (Duration::from_secs(2), Duration::from_secs(30)),
            cache_budget: 2 * 1024 * 1024 * 1024,
            auto_fetch_max: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Listening(String),
    /// A link to `peer` came up over `medium` (the transport's name). A peer
    /// can be connected over several media at once.
    PeerConnected { peer: Digest, addr: String, inbound: bool, medium: String },
    PeerDisconnected { peer: Digest, addr: String, medium: String, reason: Option<String> },
    DialFailed { addr: String, reason: String },
    Discovered { medium: Medium, addr: String },
    ObjectReceived { object: Box<VerifiedObject>, from: Digest },
    ObjectRejected { from: Digest, reason: String },
    /// The link key changed; every link was closed and will reconnect under it.
    LinkKeyRotated { id: Digest },
    /// An anonymous object left the stem phase on this node and is now public.
    Fluffed(Digest),
    /// Every chunk of an attachment arrived and the file matched its hash.
    AttachmentComplete { manifest: Digest },
    /// An attachment can never complete (e.g. its manifest is inconsistent).
    AttachmentFailed { manifest: Digest, reason: String },
}

/// An added transport and the address it listens on, if any.
type ExtraTransport = (Arc<dyn Transport>, Option<String>);

/// One link to a peer. A peer may have one link per medium.
#[derive(Clone)]
struct PeerHandle {
    peer: Digest,
    /// Transport name ("tcp", "ble", ...).
    medium: &'static str,
    /// Who dialled this link. Decides which link survives when both ends
    /// dial each other over the same medium.
    initiator: Digest,
    /// Ask the link's session to end (replaced, or the link key rotated).
    close: Arc<Notify>,
    since: Instant,
    /// Control queue (HAVE, WANT, STEM, blob requests). Sent ahead of bulk data.
    tx: mpsc::Sender<Message>,
    /// Bulk queue (OBJECT, BLOB, STASH). Order within it is preserved.
    bulk: mpsc::Sender<Message>,
    /// Peer advertised that it relays stems.
    stem_ok: bool,
}

struct Inner {
    /// The key proven on links. Never the user's posting identity.
    link_key: RwLock<Arc<Keypair>>,
    store: Mutex<Store>,
    /// Default transport: addresses without a known `medium:` prefix use it.
    transport: Arc<dyn Transport>,
    /// Further transports, each with an optional address to listen on.
    extra: Mutex<Vec<ExtraTransport>>,
    /// Pieces (STASH and BLOB) accepted per medium this run.
    by_medium: Mutex<HashMap<&'static str, u64>>,
    /// STASH pieces accepted this run, from anyone (progress signal for relays
    /// waiting on a file's pieces).
    stash_received: AtomicU64,
    config: Config,
    conns: Mutex<HashMap<u64, PeerHandle>>,
    dialing: Mutex<HashSet<String>>,
    /// Per-address retry state: (not before, current delay).
    backoff: Mutex<HashMap<String, (Instant, Duration)>>,
    /// Addresses learned from discovery this run (not persisted), with the
    /// identity pinned by the sighting if the medium carried one.
    /// Sighted addresses, with when they were last sighted.
    discovered: Mutex<HashMap<String, (Option<Digest>, Instant)>>,
    /// Identity last proven at each outbound address, to avoid redundant dials.
    addr_identity: Mutex<HashMap<String, Digest>>,
    discoveries: Mutex<Vec<Arc<dyn Discovery>>>,
    /// Current stem successor and when it was chosen.
    stem_route: Mutex<Option<(Digest, Instant)>>,
    /// Attachment fetches in progress. Lock order: `conns`, then `blobs`, then `store`.
    blobs: Mutex<fetch::Scheduler>,
    next_conn: AtomicU64,
    active: AtomicUsize,
    events: broadcast::Sender<Event>,
    shutdown: watch::Receiver<bool>,
}

pub struct Node {
    inner: Arc<Inner>,
    shutdown_tx: watch::Sender<bool>,
}

/// Handle to a started node. Dropping it does not stop the node; call [`Running::shutdown`].
pub struct Running {
    /// Where the default transport listens.
    pub local_addr: Option<String>,
    /// Every listening address, each additional transport's as `medium:addr`.
    pub listening: Vec<String>,
    tasks: JoinSet<()>,
    shutdown_tx: watch::Sender<bool>,
}

impl Running {
    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        self.tasks.shutdown().await;
    }
}

impl Node {
    /// `link_key` is what this node proves to peers on every link. Pass a key
    /// of its own, not the user's posting identity: anyone who connects learns
    /// it.
    pub fn new(link_key: Keypair, store: Store, transport: Arc<dyn Transport>, config: Config) -> Self {
        let (events, _) = broadcast::channel(1024);
        let (shutdown_tx, shutdown) = watch::channel(false);
        Node {
            inner: Arc::new(Inner {
                link_key: RwLock::new(Arc::new(link_key)),
                store: Mutex::new(store),
                transport,
                extra: Mutex::new(Vec::new()),
                by_medium: Mutex::new(HashMap::new()),
                stash_received: AtomicU64::new(0),
                config,
                conns: Mutex::new(HashMap::new()),
                dialing: Mutex::new(HashSet::new()),
                backoff: Mutex::new(HashMap::new()),
                discovered: Mutex::new(HashMap::new()),
                addr_identity: Mutex::new(HashMap::new()),
                discoveries: Mutex::new(Vec::new()),
                stem_route: Mutex::new(None),
                blobs: Mutex::new(fetch::Scheduler::default()),
                next_conn: AtomicU64::new(1),
                active: AtomicUsize::new(0),
                events,
                shutdown,
            }),
            shutdown_tx,
        }
    }

    /// The current link key (see [`Node::new`]).
    pub fn identity(&self) -> PublicKey {
        link_key(&self.inner).public()
    }

    /// Switch to a fresh random link key now. Every link is closed and
    /// reconnects under the new key, so peers see a stranger, never the old
    /// and new keys side by side. Useful when the device changes network.
    pub fn rotate_link_key(&self) {
        rotate_link_key(&self.inner);
    }

    /// Add a transport (Bluetooth, Wi-Fi Direct, ...), optionally listening on
    /// `listen`. Its addresses are written `name:addr`, where `name` is
    /// [`Transport::name`]. Call before [`Node::start`]. Peers reachable over
    /// several transports are connected over all of them.
    pub fn add_transport(&self, transport: Arc<dyn Transport>, listen: Option<String>) {
        self.inner.extra.lock().unwrap().push((transport, listen));
    }

    /// Live links as (peer, medium).
    pub fn links(&self) -> Vec<(Digest, String)> {
        let mut v: Vec<_> =
            self.inner.conns.lock().unwrap().values().map(|h| (h.peer, h.medium.to_string())).collect();
        v.sort();
        v
    }

    /// Pieces (attachment blobs and stashed blobs) accepted per medium this run.
    pub fn pieces_by_medium(&self) -> HashMap<String, u64> {
        self.inner.by_medium.lock().unwrap().iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// Register a discovery medium. Call before [`Node::start`].
    pub fn add_discovery(&self, d: Arc<dyn Discovery>) {
        self.inner.discoveries.lock().unwrap().push(d);
    }

    /// Dial an address now, optionally requiring a specific identity.
    pub fn dial(&self, addr: &str, expected: Option<Digest>) {
        spawn_dial(self.inner.clone(), addr.to_string(), expected);
    }

    /// Subscribe before [`Node::start`] to avoid missing early events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Run a closure against the store. Do not block for long inside it.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> T) -> T {
        f(&self.inner.store.lock().expect("store lock poisoned"))
    }

    /// Retain a locally authored object; it is announced on the next tick.
    pub fn publish(&self, obj: &VerifiedObject) -> Result<bool> {
        Ok(self.with_store(|s| s.put(obj, &Origin::Local))?)
    }

    /// Queue a locally authored object for stem routing: it becomes public
    /// from a relay a few hops away, not from this device. Works for anonymous
    /// objects and for signed ones (the signature says who wrote it; this hides
    /// which device they wrote it on). It is not announced by this node and is
    /// stored exactly like one relayed for others.
    ///
    /// Attachments must already be sealed into the store as
    /// [`BlobSource::Cache`] (`Store::add_file`). Every stem hop pushes the
    /// sealed blobs to the next hop before the object, so by the time the post
    /// is public the file is held all along the route, not only by the author.
    pub fn publish_private(&self, obj: &VerifiedObject) -> Result<bool> {
        self.publish_anonymous(obj)
    }

    /// Same as [`Node::publish_private`].
    pub fn publish_anonymous(&self, obj: &VerifiedObject) -> Result<bool> {
        let total: u64 = obj.object().attachments.iter().map(|a| a.size).sum();
        if total > self.inner.config.max_anonymous_attachments {
            bail!(
                "anonymous attachments total {total} bytes; the limit is {} (each relay on the route carries the whole file)",
                self.inner.config.max_anonymous_attachments
            );
        }
        self.with_store(|s| {
            for a in &obj.object().attachments {
                let blobs = s.attachment_blobs(&a.into()).context("attachment is not sealed into this store")?;
                for b in blobs {
                    if s.blob_source(&b)? != Some(BlobSource::Cache) {
                        bail!("private-origin attachments must be stored as cache, not own: that would mark this node as the author");
                    }
                }
            }
            Ok(s.put_stem(obj, true)?)
        })
    }

    /// Blobs accepted from each peer during this run (in memory only).
    pub fn blob_sources(&self) -> HashMap<Digest, usize> {
        self.inner.blobs.lock().unwrap().received_from.clone()
    }

    /// Queue an object's attachments for fetching. The running node fetches them
    /// from connected peers; watch for [`Event::AttachmentComplete`].
    pub fn fetch_attachments(&self, obj: &VerifiedObject) -> Result<()> {
        self.with_store(|s| {
            for a in &obj.object().attachments {
                s.want_attachment(&a.into())?;
            }
            Ok(())
        })
    }

    /// Attachments currently being fetched.
    pub fn active_fetches(&self) -> usize {
        self.inner.blobs.lock().unwrap().active()
    }

    pub fn connected_peers(&self) -> Vec<Digest> {
        let mut v: Vec<_> = self.inner.conns.lock().unwrap().values().map(|h| h.peer).collect();
        v.sort();
        v.dedup();
        v
    }

    pub async fn start(&self) -> Result<Running> {
        let mut tasks = JoinSet::new();
        let mut local_addr = None;
        let mut listening = Vec::new();

        if let Some(addr) = &self.inner.config.listen {
            let listener = self.inner.transport.listen(addr).await.with_context(|| format!("listening on {addr}"))?;
            let bound = listener.local_addr();
            emit(&self.inner, Event::Listening(bound.clone()));
            local_addr = Some(bound.clone());
            listening.push(bound);
            tasks.spawn(accept_loop(self.inner.clone(), listener, self.inner.transport.name()));
        }
        let extra: Vec<_> = self.inner.extra.lock().unwrap().clone();
        for (t, listen) in extra {
            let Some(addr) = listen else { continue };
            let listener = t.listen(&addr).await.with_context(|| format!("listening on {}:{addr}", t.name()))?;
            let bound = format!("{}:{}", t.name(), listener.local_addr());
            emit(&self.inner, Event::Listening(bound.clone()));
            listening.push(bound);
            tasks.spawn(accept_loop(self.inner.clone(), listener, t.name()));
        }

        let discoveries: Vec<_> = self.inner.discoveries.lock().unwrap().clone();
        if !discoveries.is_empty() {
            let port = local_addr.as_deref().and_then(|a| a.rsplit_once(':')).and_then(|(_, p)| p.parse().ok());
            // A loopback-only listener can't be reached from the LAN, so don't advertise it.
            let loopback = local_addr.as_deref().is_some_and(|a| a.starts_with("127.") || a.starts_with("[::1]"));
            // Port 0: nothing reachable over IP, but radio media still announce.
            let port = if loopback { 0 } else { port.unwrap_or(0) };
            let beacon = self.inner.config.beacon.then(|| Beacon { instance: random_bytes(), port });
            let (tx, mut rx) = mpsc::channel::<Sighting>(256);
            for d in discoveries {
                let tx = tx.clone();
                tasks.spawn(async move {
                    let _ = d.run(beacon, tx).await;
                });
            }
            let inner = self.inner.clone();
            tasks.spawn(async move {
                while let Some(s) = rx.recv().await {
                    let fresh = {
                        let mut d = inner.discovered.lock().unwrap();
                        if d.len() >= 1024 && !d.contains_key(&s.addr) {
                            continue; // bounded: a hostile medium can't grow this forever
                        }
                        d.insert(s.addr.clone(), (s.identity, Instant::now())).is_none()
                    };
                    if fresh {
                        emit(&inner, Event::Discovered { medium: s.medium, addr: s.addr.clone() });
                    }
                    if !already_connected(&inner, &s.addr) {
                        spawn_dial(inner.clone(), s.addr, s.identity);
                    }
                }
            });
        }

        let start_seq = self.with_store(|s| s.max_seq())?;
        tasks.spawn(announcer(self.inner.clone(), start_seq));
        tasks.spawn(dialer(self.inner.clone()));
        tasks.spawn(stem_pump(self.inner.clone()));
        tasks.spawn(fetch::pump(self.inner.clone()));
        tasks.spawn(cover_pump(self.inner.clone()));
        tasks.spawn(chatter_pump(self.inner.clone()));
        tasks.spawn(cache_pump(self.inner.clone()));
        if let Some(every) = self.inner.config.link_key_rotation {
            let inner = self.inner.clone();
            tasks.spawn(async move {
                loop {
                    let jitter = rand::thread_rng().gen_range(0.9..1.1);
                    tokio::time::sleep(every.mul_f64(jitter)).await;
                    rotate_link_key(&inner);
                }
            });
        }

        Ok(Running { local_addr, listening, tasks, shutdown_tx: self.shutdown_tx.clone() })
    }
}

async fn accept_loop(inner: Arc<Inner>, mut listener: Box<dyn Listener>, medium: &'static str) {
    loop {
        let Ok(acc) = listener.accept().await else { continue };
        if inner.active.load(Ordering::Relaxed) >= inner.config.max_connections {
            continue; // drop: over budget
        }
        let inner = inner.clone();
        tokio::spawn(async move {
            let _ = run_connection(inner, acc.stream, acc.remote, true, None, medium).await;
        });
    }
}

/// The transport for `addr`, the address in that transport's own terms, and
/// its medium name. `ble:XX` goes to a transport named "ble"; an address with
/// no known prefix goes to the default transport.
fn resolve(inner: &Inner, addr: &str) -> (Arc<dyn Transport>, String, &'static str) {
    if let Some((scheme, rest)) = addr.split_once(':') {
        if let Some((t, _)) = inner.extra.lock().unwrap().iter().find(|(t, _)| t.name() == scheme) {
            return (t.clone(), rest.to_string(), t.name());
        }
        if scheme == inner.transport.name() {
            return (inner.transport.clone(), rest.to_string(), inner.transport.name());
        }
    }
    (inner.transport.clone(), addr.to_string(), inner.transport.name())
}

fn emit(inner: &Inner, e: Event) {
    let _ = inner.events.send(e);
}

fn link_key(inner: &Inner) -> Arc<Keypair> {
    inner.link_key.read().unwrap().clone()
}

fn rotate_link_key(inner: &Inner) {
    let key = Keypair::generate();
    let id = key.public().id();
    *inner.link_key.write().unwrap() = Arc::new(key);
    // Drop every link: a peer must not see the old key and the new one at once.
    for h in inner.conns.lock().unwrap().drain().map(|(_, h)| h) {
        h.close.notify_one();
    }
    inner.addr_identity.lock().unwrap().clear();
    *inner.stem_route.lock().unwrap() = None;
    // Radios change their hardware address too, or it would link the two keys.
    inner.transport.rotate_address();
    for (t, _) in inner.extra.lock().unwrap().iter() {
        t.rotate_address();
    }
    emit(inner, Event::LinkKeyRotated { id });
}

fn keepalive_pad() -> Message {
    Message::Pad(vec![0u8; rand::thread_rng().gen_range(16..256)])
}

fn my_hello(keypair: &Keypair, binding: &[u8; 32], role: Role, flags: u8) -> Message {
    let public_key = keypair.public();
    let signature = keypair.sign(&hello_input(binding, role, PROTOCOL_VERSION, flags, &public_key));
    Message::Hello { protocol: PROTOCOL_VERSION, flags, public_key, signature }
}

/// Read the peer's HELLO and check its proof against this channel.
async fn read_hello<R: AsyncRead + Unpin>(
    keypair: &Keypair,
    binding: &[u8; 32],
    peer_role: Role,
    expected: Option<&Digest>,
    rd: &mut R,
) -> Result<(PublicKey, u8)> {
    let (protocol, flags, public_key, signature) = match read_message(rd).await? {
        Some(Message::Hello { protocol, flags, public_key, signature }) => (protocol, flags, public_key, signature),
        Some(_) => bail!("expected HELLO"),
        None => bail!("peer closed during handshake"),
    };
    if protocol != PROTOCOL_VERSION {
        bail!("peer speaks protocol {protocol}, we speak {PROTOCOL_VERSION}");
    }
    public_key
        .verify(&hello_input(binding, peer_role, protocol, flags, &public_key), &signature)
        .context("peer failed key-possession proof")?;
    if public_key == keypair.public() {
        bail!("connected to self");
    }
    if let Some(want) = expected {
        if *want != public_key.id() {
            bail!("identity mismatch: expected {}, peer proved {}", want.short(), public_key.id().short());
        }
    }
    Ok((public_key, flags))
}

/// Mutual authentication inside an established encrypted channel.
///
/// The responder proves its identity first. The initiator checks that proof,
/// including `expected` if the caller pinned an identity, before revealing its
/// own. A dialler therefore never discloses who it is to an impostor holding a
/// pinned address. Returns the peer's verified key and HELLO flags.
pub async fn handshake<R, W>(
    keypair: &Keypair,
    flags: u8,
    role: Role,
    binding: &[u8; 32],
    expected: Option<&Digest>,
    rd: &mut R,
    wr: &mut W,
) -> Result<(PublicKey, u8)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match role {
        Role::Responder => {
            write_message(wr, &my_hello(keypair, binding, role, flags)).await?;
            read_hello(keypair, binding, role.peer(), expected, rd).await
        }
        Role::Initiator => {
            let peer = read_hello(keypair, binding, role.peer(), expected, rd).await?;
            write_message(wr, &my_hello(keypair, binding, role, flags)).await?;
            Ok(peer)
        }
    }
}

/// Connect, learn the identity behind `addr`, and disconnect, without
/// revealing our own identity (we hang up before sending HELLO). Used by
/// `mesh peer add`.
pub async fn probe(transport: &dyn Transport, keypair: &Keypair, addr: &str, timeout: Duration) -> Result<PublicKey> {
    let stream = transport.dial(addr).await.with_context(|| format!("connecting to {addr}"))?;
    let (pk, _) = tokio::time::timeout(timeout, async {
        let mut ch = secure(stream, Role::Initiator).await?;
        read_hello(keypair, &ch.binding, Role::Responder, None, &mut ch.stream).await
    })
    .await
    .context("handshake timed out")??;
    Ok(pk)
}

struct ActiveGuard<'a>(&'a AtomicUsize);
impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn run_connection(
    inner: Arc<Inner>,
    stream: Stream,
    addr: String,
    inbound: bool,
    expected: Option<Digest>,
    medium: &'static str,
) -> Result<()> {
    inner.active.fetch_add(1, Ordering::Relaxed);
    let _guard = ActiveGuard(&inner.active);

    let my_flags = if inner.config.relay { FLAG_ACCEPTS_STEM } else { 0 };
    let role = if inbound { Role::Responder } else { Role::Initiator };
    // The key this link is proven with; a later rotation closes the link.
    let key = link_key(&inner);
    let (mut rd, mut wr, peer_pk, peer_flags) = tokio::time::timeout(inner.config.handshake_timeout, async {
        let ch = secure(stream, role).await?;
        let (mut rd, mut wr) = tokio::io::split(ch.stream);
        let (pk, flags) =
            handshake(&key, my_flags, role, &ch.binding, expected.as_ref(), &mut rd, &mut wr).await?;
        anyhow::Ok((rd, wr, pk, flags))
    })
    .await
    .context("handshake timed out")?
    .with_context(|| format!("handshake with {addr}"))?;
    let peer = peer_pk.id();
    if !inbound {
        inner.addr_identity.lock().unwrap().insert(addr.clone(), peer);
    }

    // Two queues: control messages always go out before bulk data, so a stream
    // of chunks doesn't hold up HAVE/WANT (head-of-line blocking).
    let (tx, mut rx) = mpsc::channel::<Message>(1024);
    let (bulk, mut bulk_rx) = mpsc::channel::<Message>(64);
    let keepalive = inner.config.keepalive;
    let writer = tokio::spawn(async move {
        loop {
            let m = tokio::select! {
                biased;
                m = rx.recv() => m,
                m = bulk_rx.recv() => m,
                // Jittered, so keepalives aren't a steady beat that a real
                // message stands out against.
                _ = tokio::time::sleep(keepalive.mul_f64(rand::random::<f64>() + 0.5)) => Some(keepalive_pad()),
            };
            let Some(m) = m else { break };
            if write_message(&mut wr, &m).await.is_err() {
                break;
            }
        }
    });

    let conn_id = inner.next_conn.fetch_add(1, Ordering::Relaxed);
    let stem_ok = peer_flags & FLAG_ACCEPTS_STEM != 0;
    let me = key.public().id();
    let initiator = if inbound { peer } else { me };
    let close = Arc::new(Notify::new());
    let handle = PeerHandle {
        peer,
        medium,
        initiator,
        close: close.clone(),
        since: Instant::now(),
        tx: tx.clone(),
        bulk: bulk.clone(),
        stem_ok,
    };
    {
        // One link per peer per medium.
        // - Dial race (opened from opposite ends moments apart): both ends keep
        //   the link opened by the smaller identity, agreeing without talking.
        // - Otherwise the newer link wins. A peer only dials while it has no
        //   live link, so the old one is most likely dead (it walked out of
        //   range and back) even if we haven't noticed yet.
        let mut conns = inner.conns.lock().unwrap();
        if link_key(&inner).public().id() != me {
            drop(conns);
            writer.abort();
            return Ok(()); // key rotated during the handshake
        }
        let dup = conns.iter().find(|(_, h)| h.peer == peer && h.medium == medium).map(|(id, h)| (*id, h.clone()));
        if let Some((old_id, old)) = dup {
            let race = old.initiator != initiator && old.since.elapsed() < inner.config.link_race_window;
            let keep_new = !race || initiator == me.min(peer);
            if keep_new {
                old.close.notify_one();
                conns.remove(&old_id);
            } else {
                drop(conns);
                writer.abort();
                return Ok(());
            }
        }
        conns.insert(conn_id, handle);
    }
    if !inbound {
        let _ = inner.store.lock().unwrap().touch_peer(&addr);
    }
    emit(&inner, Event::PeerConnected { peer, addr: addr.clone(), inbound, medium: medium.to_string() });

    let result = session(&inner, conn_id, peer, medium, &close, &mut rd, &tx, &bulk).await;

    inner.conns.lock().unwrap().remove(&conn_id);
    writer.abort();
    emit(
        &inner,
        Event::PeerDisconnected {
            peer,
            addr,
            medium: medium.to_string(),
            reason: result.as_ref().err().map(|e| format!("{e:#}")),
        },
    );
    result
}

#[allow(clippy::too_many_arguments)]
async fn session<R: AsyncRead + Unpin>(
    inner: &Arc<Inner>,
    conn_id: u64,
    peer: Digest,
    medium: &'static str,
    close: &Notify,
    rd: &mut R,
    tx: &mpsc::Sender<Message>,
    bulk: &mpsc::Sender<Message>,
) -> Result<()> {
    // Initial inventory.
    let ids = inner.store.lock().unwrap().shareable_ids(inner.config.relay)?;
    for chunk in ids.chunks(MAX_IDS_PER_MESSAGE) {
        tx.send(Message::Have(chunk.to_vec())).await?;
    }

    let mut wanted: HashMap<Digest, Instant> = HashMap::new();
    let mut strikes = 0u32;
    let mut stem_window = (Instant::now(), 0u32);
    let mut stash_window = (Instant::now(), 0u32);
    let mut strike = |reason: String| -> Result<()> {
        emit(inner, Event::ObjectRejected { from: peer, reason });
        strikes += 1;
        if strikes >= inner.config.max_strikes {
            bail!("too many invalid or unsolicited objects");
        }
        Ok(())
    };
    let mut shutdown = inner.shutdown.clone();
    if *shutdown.borrow() {
        return Ok(());
    }

    loop {
        let msg = tokio::select! {
            m = tokio::time::timeout(inner.config.link_timeout, read_message(rd)) => match m {
                Ok(m) => m?,
                Err(_) => bail!("link silent for {:?}", inner.config.link_timeout),
            },
            _ = shutdown.changed() => return Ok(()),
            _ = close.notified() => return Ok(()), // replaced, or our link key rotated
        };
        let Some(msg) = msg else { return Ok(()) };
        match msg {
            Message::Have(ids) => {
                let now = Instant::now();
                wanted.retain(|_, t| now.duration_since(*t) < inner.config.want_timeout);
                let mut want = Vec::new();
                let mut gone_public = Vec::new();
                {
                    let store = inner.store.lock().unwrap();
                    for id in ids {
                        if store.has(&id)? {
                            // Someone else is offering an object we hold under embargo:
                            // it has been fluffed elsewhere, so it is public now.
                            // Announcing it instantly (without having fetched it) would
                            // mark us as a stem node, so requeue it like a normal arrival.
                            if store.is_embargoed(&id)? {
                                gone_public.push(id);
                            }
                            continue;
                        }
                        if wanted.len() < inner.config.max_pending_wants && !wanted.contains_key(&id) {
                            wanted.insert(id, now);
                            want.push(id);
                        }
                    }
                }
                if !gone_public.is_empty() {
                    let store = inner.store.lock().unwrap();
                    for id in gone_public {
                        store.make_public_quietly(&id)?;
                    }
                }
                if !want.is_empty() {
                    tx.send(Message::Want(want)).await?;
                }
            }
            Message::Want(ids) => {
                let mut out = Vec::new();
                {
                    let store = inner.store.lock().unwrap();
                    for id in ids {
                        // Covers embargo: a peer guessing the ID of a stem-phase object gets nothing.
                        if !store.shareable(&id, inner.config.relay)? {
                            continue;
                        }
                        if let Some(obj) = store.get(&id)? {
                            out.push(obj.bytes().to_vec());
                        }
                    }
                }
                for b in out {
                    bulk.send(Message::Object(b)).await?;
                }
            }
            Message::Object(bytes) => {
                let verdict = match VerifiedObject::decode(&bytes) {
                    Err(e) => Err(format!("invalid object: {e}")),
                    Ok(obj) if wanted.remove(&obj.id()).is_none() => Err(format!("unsolicited object {}", obj.id().short())),
                    Ok(obj) => Ok(obj),
                };
                match verdict {
                    Ok(obj) => {
                        // Retention policy hook: v0.1 retains every valid object it asked for.
                        let is_new = inner.store.lock().unwrap().put(&obj, &Origin::Peer(peer))?;
                        if is_new && inner.config.relay {
                            // Keep attachments available for others, within budget.
                            let store = inner.store.lock().unwrap();
                            for a in &obj.object().attachments {
                                if a.size <= inner.config.auto_fetch_max {
                                    store.want_attachment(&a.into())?;
                                }
                            }
                        }
                        if is_new {
                            emit(inner, Event::ObjectReceived { object: Box::new(obj), from: peer });
                        }
                    }
                    Err(reason) => strike(reason)?,
                }
            }
            Message::Stem(bytes) => {
                if !inner.config.relay {
                    strike("STEM sent to a node that does not relay".into())?;
                    continue;
                }
                if stem_window.0.elapsed() >= Duration::from_secs(60) {
                    stem_window = (Instant::now(), 0);
                }
                stem_window.1 += 1;
                if stem_window.1 > inner.config.max_stems_per_minute {
                    strike("STEM rate limit exceeded".into())?;
                    continue;
                }
                let obj = match VerifiedObject::decode(&bytes) {
                    Ok(o) => o,
                    Err(e) => {
                        strike(format!("invalid stem object: {e}"))?;
                        continue;
                    }
                };
                let id = obj.id();
                let (had, embargoed) = {
                    let s = inner.store.lock().unwrap();
                    (s.has(&id)?, s.is_embargoed(&id)?)
                };
                if had {
                    // A stem that loops back to us ends here.
                    if embargoed {
                        fluff(inner, &id);
                    }
                    continue;
                }
                inner.store.lock().unwrap().put_stem(&obj, false)?;
                emit(inner, Event::ObjectReceived { object: Box::new(obj.clone()), from: peer });
                if awaiting_pieces(inner, &obj) {
                    // Some pieces are still coming over another (slower) link.
                    let inner = inner.clone();
                    tokio::spawn(async move {
                        await_pieces(&inner, &obj).await;
                        route_stem(&inner, &obj, Some(peer));
                    });
                } else {
                    route_stem(inner, &obj, Some(peer));
                }
            }
            Message::Stash(data) => {
                if !inner.config.relay {
                    strike("STASH sent to a node that does not relay".into())?;
                    continue;
                }
                if stash_window.0.elapsed() >= Duration::from_secs(60) {
                    stash_window = (Instant::now(), 0);
                }
                stash_window.1 += 1;
                if stash_window.1 > inner.config.max_stash_per_minute {
                    strike("STASH rate limit exceeded".into())?;
                    continue;
                }
                if data.len() != BLOB_SIZE {
                    strike("STASH blob of wrong size".into())?;
                    continue;
                }
                // Opaque to us: we can't read it or tell what it belongs to. Held
                // as cache like any other blob; the matching STEM follows.
                inner.store.lock().unwrap().put_blob(&data, BlobSource::Cache)?;
                inner.stash_received.fetch_add(1, Ordering::Relaxed);
                count_piece(inner, medium);
            }
            Message::BlobWant(ids) => {
                let mut missing = Vec::new();
                for id in ids {
                    let data = {
                        let store = inner.store.lock().unwrap();
                        if store.blob_shareable(&id, inner.config.relay)? { store.get_blob(&id)? } else { None }
                    };
                    // Never block here: if our bulk queue is full, say so and let
                    // the requester try another peer.
                    let sent = data.is_some_and(|d| bulk.try_send(Message::Blob(d)).is_ok());
                    if !sent {
                        missing.push(id);
                    }
                }
                if !missing.is_empty() {
                    tx.send(Message::BlobMissing(missing)).await?;
                }
            }
            Message::Blob(data) => {
                if fetch::on_blob(inner, peer, &data) {
                    count_piece(inner, medium);
                } else {
                    fetch::on_junk(inner, conn_id);
                    strike(format!("unsolicited blob {}", Digest::of(&data).short()))?;
                }
            }
            Message::BlobMissing(ids) => fetch::on_missing(inner, conn_id, &ids),
            Message::Pad(_) => {} // cover traffic
            Message::Hello { .. } => bail!("unexpected handshake message"),
        }
    }
}

/// Periodically offer newly retained objects to every connected peer.
async fn announcer(inner: Arc<Inner>, mut last_seq: i64) {
    let mut tick = tokio::time::interval(inner.config.announce_interval);
    loop {
        tick.tick().await;
        let entries = match inner.store.lock().unwrap().entries_since(last_seq) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let Some(last) = entries.last() else { continue };
        last_seq = last.seq;

        let targets: Vec<(Digest, mpsc::Sender<Message>)> =
            one_link_per_peer(&inner).into_iter().map(|h| (h.peer, h.tx)).collect();
        for (peer, tx) in targets {
            let ids: Vec<Digest> = entries
                .iter()
                .filter(|e| {
                    Store::is_shareable(&e.origin, e.embargoed, inner.config.relay) && e.origin != Origin::Peer(peer)
                })
                .map(|e| e.id)
                .collect();
            for chunk in ids.chunks(MAX_IDS_PER_MESSAGE) {
                // Best effort: a congested peer catches up from inventory on reconnect.
                let _ = tx.try_send(Message::Have(chunk.to_vec()));
            }
        }
    }
}

/// Offer one object to every connected peer.
fn broadcast_have(inner: &Inner, id: Digest) {
    for h in one_link_per_peer(inner) {
        let _ = h.tx.try_send(Message::Have(vec![id]));
    }
}

/// A random live link to each connected peer.
fn one_link_per_peer(inner: &Inner) -> Vec<PeerHandle> {
    let conns = inner.conns.lock().unwrap();
    let mut by_peer: HashMap<Digest, Vec<&PeerHandle>> = HashMap::new();
    for h in conns.values() {
        by_peer.entry(h.peer).or_default().push(h);
    }
    let mut rng = rand::thread_rng();
    by_peer.into_values().map(|v| v[rng.gen_range(0..v.len())].clone()).collect()
}

/// Every live link to `peer` that accepts stems.
fn stem_links(conns: &HashMap<u64, PeerHandle>, peer: Digest) -> Vec<PeerHandle> {
    conns.values().filter(|h| h.peer == peer && h.stem_ok).cloned().collect()
}

fn count_piece(inner: &Inner, medium: &'static str) {
    *inner.by_medium.lock().unwrap().entry(medium).or_default() += 1;
}

/// Hand `msgs` to a peer over all of `links` at once. Each link takes the next
/// message when it is free and is paced on its own, so a fast medium carries
/// more and an observer of one medium sees only part of the run. Returns
/// `false` if something could not be handed over (every link failed).
async fn push_spread(inner: &Arc<Inner>, links: &[PeerHandle], msgs: Vec<Message>) -> bool {
    let queue = Arc::new(Mutex::new(VecDeque::from(msgs)));
    let mut set = JoinSet::new();
    for l in links {
        let (inner, queue, bulk) = (inner.clone(), queue.clone(), l.bulk.clone());
        set.spawn(async move {
            loop {
                let Some(m) = queue.lock().unwrap().pop_front() else { return };
                pace(&inner.config).await;
                if let Err(SendError(m)) = bulk.send(m).await {
                    queue.lock().unwrap().push_front(m); // another link takes it
                    return;
                }
            }
        });
    }
    while set.join_next().await.is_some() {}
    let empty = queue.lock().unwrap().is_empty();
    empty
}

/// Send `msg` on one of `links`, trying them in random order.
async fn send_any(links: &[PeerHandle], msg: Message, control: bool) -> bool {
    let mut order: Vec<usize> = (0..links.len()).collect();
    {
        use rand::seq::SliceRandom;
        order.shuffle(&mut rand::thread_rng());
    }
    let mut msg = msg;
    for i in order {
        let q = if control { &links[i].tx } else { &links[i].bulk };
        match q.send(msg).await {
            Ok(()) => return true,
            Err(SendError(m)) => msg = m,
        }
    }
    false
}

/// End the stem phase for `id` on this node and announce it.
fn fluff(inner: &Inner, id: &Digest) {
    let cleared = inner.store.lock().unwrap().clear_embargo(id).unwrap_or(false);
    if cleared {
        broadcast_have(inner, *id);
        emit(inner, Event::Fluffed(*id));
    }
}

fn embargo_deadline(cfg: &Config) -> u64 {
    embargo_deadline_after(cfg, 0)
}

/// Embargo for an object whose stem push carried `pieces` blobs. Each later hop
/// spends about as long again pushing them, so the fallback timer allows for a
/// few such hops. Otherwise the author's timer could fire while the file is
/// still moving downstream, and the author would make its own post public.
fn embargo_deadline_after(cfg: &Config, pieces: usize) -> u64 {
    let (lo, hi) = (cfg.stem_embargo.0.as_millis() as u64, cfg.stem_embargo.1.as_millis() as u64);
    let per_hop = pieces as u64 * cfg.stash_pace.1.as_millis() as u64;
    mesh_object::now_ms() + rand::thread_rng().gen_range(lo..=hi.max(lo)) + 4 * per_hop
}

/// Wait one STASH gap.
async fn pace(cfg: &Config) {
    let (lo, hi) = cfg.stash_pace;
    let gap = if hi > lo { rand::thread_rng().gen_range(lo..=hi) } else { lo };
    tokio::time::sleep(gap).await;
}

/// The peer to pass stems to, never `exclude` (the peer we got it from).
/// The successor is kept for a whole epoch so routes are stable.
/// Returns all of its stem-capable links.
fn stem_successor(inner: &Inner, exclude: Option<Digest>) -> Option<Vec<PeerHandle>> {
    let conns = inner.conns.lock().unwrap();
    let mut route = inner.stem_route.lock().unwrap();
    let usable = |p: &Digest| Some(*p) != exclude && conns.values().any(|h| h.peer == *p && h.stem_ok);
    if let Some((p, chosen)) = *route {
        if chosen.elapsed() < inner.config.stem_epoch && usable(&p) {
            return Some(stem_links(&conns, p));
        }
    }
    let mut candidates: Vec<Digest> = conns.values().filter(|h| usable(&h.peer)).map(|h| h.peer).collect();
    candidates.sort();
    candidates.dedup();
    if candidates.is_empty() {
        return None;
    }
    let pick = candidates[rand::thread_rng().gen_range(0..candidates.len())];
    // Replace the epoch route only if it is expired or gone, not merely excluded.
    let current_ok = route.is_some_and(|(p, t)| {
        t.elapsed() < inner.config.stem_epoch && conns.values().any(|h| h.peer == p && h.stem_ok)
    });
    if !current_ok {
        *route = Some((pick, Instant::now()));
    }
    Some(stem_links(&conns, pick))
}

/// Sealed blobs an anonymous object must carry along the stem. `None` if we
/// don't hold all of them (or they exceed the anonymous budget), in which case
/// the object can't continue down the stem with its file.
fn stem_blobs(inner: &Inner, obj: &VerifiedObject) -> Option<Vec<Vec<u8>>> {
    let atts = &obj.object().attachments;
    if atts.iter().map(|a| a.size).sum::<u64>() > inner.config.max_anonymous_attachments {
        return None;
    }
    let store = inner.store.lock().unwrap();
    let mut out = Vec::new();
    for a in atts {
        for id in store.attachment_blobs(&a.into()).ok()? {
            out.push(store.get_blob(&id).ok()??);
        }
    }
    Some(out)
}

/// Advance an embargoed object one step. `from = None` means it came from our
/// outbox; the author never fluffs at hop 0. Returns `false` if it could not
/// move yet (author with no stem-capable peer).
///
/// With attachments, the sealed blobs go first (STASH, paced exactly like
/// cover traffic, spread over every link to the next hop) and the object
/// after them, so the next hop holds the whole file before it passes the post
/// on.
/// The fallback timer starts only once the object has been handed over.
fn route_stem(inner: &Arc<Inner>, obj: &VerifiedObject, from: Option<Digest>) -> bool {
    let fluff_now = from.is_some() && rand::thread_rng().gen_bool(inner.config.stem_fluff_probability.clamp(0.0, 1.0));
    if !fluff_now {
        match stem_blobs(inner, obj) {
            Some(blobs) => {
                if let Some(links) = stem_successor(inner, from) {
                    if blobs.is_empty() {
                        let mut order: Vec<usize> = (0..links.len()).collect();
                        {
                            use rand::seq::SliceRandom;
                            order.shuffle(&mut rand::thread_rng());
                        }
                        if order.iter().any(|&i| links[i].tx.try_send(Message::Stem(obj.bytes().to_vec())).is_ok()) {
                            let _ = inner.store.lock().unwrap().set_embargo(&obj.id(), embargo_deadline(&inner.config));
                            return true;
                        }
                    } else {
                        let (inner, id, stem, author) = (inner.clone(), obj.id(), obj.bytes().to_vec(), from.is_none());
                        tokio::spawn(async move {
                            let pieces = blobs.len();
                            let stash = blobs.into_iter().map(Message::Stash).collect();
                            let mut delivered = push_spread(&inner, &links, stash).await;
                            // The STEM may overtake pieces still queued on a slower
                            // link; the next hop waits for them (await_pieces).
                            delivered = delivered && send_any(&links, Message::Stem(stem), false).await;
                            let store = inner.store.lock().unwrap();
                            if delivered {
                                let _ = store.set_embargo(&id, embargo_deadline_after(&inner.config, pieces));
                            } else if author {
                                // Never fall back to publishing our own post: try another route.
                                let _ = store.stem_outbox_add(&id);
                            } else {
                                let _ = store.set_embargo(&id, embargo_deadline(&inner.config));
                            }
                        });
                        return true;
                    }
                }
            }
            // A relay missing part of the file can't pass it on: end the stem
            // here. Holders upstream serve the missing pieces once it is public.
            None if from.is_some() => {}
            None => return false,
        }
        if from.is_none() {
            return false;
        }
    }
    fluff(inner, &obj.id());
    true
}

/// True if `obj` carries a file we are still receiving pieces of, within the
/// anonymous budget (over budget it can never continue, so don't wait).
fn awaiting_pieces(inner: &Inner, obj: &VerifiedObject) -> bool {
    let atts = &obj.object().attachments;
    !atts.is_empty()
        && atts.iter().map(|a| a.size).sum::<u64>() <= inner.config.max_anonymous_attachments
        && !holds_all_pieces(inner, obj)
}

fn holds_all_pieces(inner: &Inner, obj: &VerifiedObject) -> bool {
    let store = inner.store.lock().unwrap();
    obj.object().attachments.iter().all(|a| {
        matches!(store.manifest(&a.object, &a.key), Ok(Some(m)) if store.missing_chunks(&m).is_ok_and(|v| v.is_empty()))
    })
}

/// Wait while pieces keep arriving, until `obj`'s file is complete. Gives up
/// after `stem_assembly_idle` with no new piece, or ten minutes in all.
async fn await_pieces(inner: &Arc<Inner>, obj: &VerifiedObject) {
    let started = Instant::now();
    let mut seen = inner.stash_received.load(Ordering::Relaxed);
    let mut last_progress = Instant::now();
    let mut last_check = Instant::now();
    // The last pieces may have landed before we started counting, so check
    // now, then whenever a piece arrives, and at least once a second.
    if holds_all_pieces(inner, obj) {
        return;
    }
    while !*inner.shutdown.borrow() && started.elapsed() < Duration::from_secs(600) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = inner.stash_received.load(Ordering::Relaxed);
        let progressed = now != seen;
        if progressed {
            seen = now;
            last_progress = Instant::now();
        }
        if progressed || last_check.elapsed() >= Duration::from_secs(1) {
            last_check = Instant::now();
            if holds_all_pieces(inner, obj) {
                return;
            }
        }
        if last_progress.elapsed() >= inner.config.stem_assembly_idle {
            return;
        }
    }
}

/// Push our own anonymous objects into the stem, and fluff any embargo that ran out.
async fn stem_pump(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(inner.config.announce_interval);
    // When each outbox entry may leave (author delay), in memory only.
    let mut not_before: HashMap<Digest, Instant> = HashMap::new();
    loop {
        tick.tick().await;
        let pending: Vec<(Digest, Option<VerifiedObject>)> = {
            let s = inner.store.lock().unwrap();
            s.stem_outbox()
                .unwrap_or_default()
                .into_iter()
                .map(|id| (id, s.get(&id).ok().flatten()))
                .collect()
        };
        let now = Instant::now();
        not_before.retain(|id, _| pending.iter().any(|(p, _)| p == id));
        for (id, obj) in pending {
            let (lo, hi) = inner.config.author_delay;
            let at = *not_before
                .entry(id)
                .or_insert_with(|| now + if hi > lo { rand::thread_rng().gen_range(lo..=hi) } else { lo });
            if now < at {
                continue;
            }
            // Take it out first: a failed push puts it back.
            let _ = inner.store.lock().unwrap().stem_outbox_remove(&id);
            let done = match obj {
                Some(o) => route_stem(&inner, &o, None),
                None => true,
            };
            if !done {
                let _ = inner.store.lock().unwrap().stem_outbox_add(&id);
            }
        }
        let expired = inner.store.lock().unwrap().expired_embargoes(mesh_object::now_ms()).unwrap_or_default();
        for id in expired {
            fluff(&inner, &id);
        }
    }
}

/// Decoy runs of STASH at random times and lengths, paced like real ones, to
/// relaying peers. Pieces are random bytes, indistinguishable from sealed
/// blobs. A run ends with PAD, as a real run ends with STEM. Receivers can't
/// tell decoys from real pieces and keep them as cache (evicted first).
async fn cover_pump(inner: Arc<Inner>) {
    let cfg = &inner.config;
    if cfg.cover_bytes_per_hour == 0 || cfg.cover_max_run == 0 {
        return;
    }
    loop {
        // Run length is log-uniform, so short and long runs are both common.
        let (run, wait) = {
            let mut rng = rand::thread_rng();
            let max = cfg.cover_max_run as f64;
            let run = (max.ln() * rng.gen::<f64>()).exp().round().clamp(1.0, max) as usize;
            // Poisson timing at the configured average rate.
            let mean = run as f64 * BLOB_SIZE as f64 * 3600.0 / cfg.cover_bytes_per_hour as f64;
            let wait = -mean * (1.0 - rng.gen::<f64>()).ln();
            (run, Duration::from_secs_f64(wait.min(86_400.0)))
        };
        tokio::time::sleep(wait).await;
        // One peer, all of its links, spread exactly like a real run.
        let links = {
            let conns = inner.conns.lock().unwrap();
            let peers: Vec<Digest> = conns.values().filter(|h| h.stem_ok).map(|h| h.peer).collect();
            if peers.is_empty() {
                continue;
            }
            stem_links(&conns, peers[rand::thread_rng().gen_range(0..peers.len())])
        };
        let pieces: Vec<Message> = (0..run)
            .map(|_| {
                let mut piece = vec![0u8; BLOB_SIZE];
                rand::thread_rng().fill(&mut piece[..]);
                Message::Stash(piece)
            })
            .collect();
        if push_spread(&inner, &links, pieces).await {
            let pad = vec![0u8; rand::thread_rng().gen_range(128..1024)];
            let _ = send_any(&links, Message::Pad(pad), false).await;
        }
    }
}

/// Decoy single messages at random times, sized like posts. A STEM leaving
/// the author is a message nothing on the link triggered; with these, every
/// node sends such messages all the time. Sent on the control queue, as a
/// STEM is.
async fn chatter_pump(inner: Arc<Inner>) {
    let per_hour = inner.config.cover_messages_per_hour;
    if per_hour == 0 {
        return;
    }
    let mean = 3600.0 / per_hour as f64;
    loop {
        let wait = -mean * (1.0 - rand::random::<f64>()).ln();
        tokio::time::sleep(Duration::from_secs_f64(wait.min(86_400.0))).await;
        let target = {
            let conns = inner.conns.lock().unwrap();
            let ok: Vec<_> = conns.values().filter(|h| h.stem_ok).map(|h| h.tx.clone()).collect();
            if ok.is_empty() {
                continue;
            }
            ok[rand::thread_rng().gen_range(0..ok.len())].clone()
        };
        // Log-uniform 200..4096 bytes: short and long posts both common.
        let len = (200f64.ln() + rand::random::<f64>() * (4096f64.ln() - 200f64.ln())).exp() as usize;
        let _ = target.try_send(Message::Pad(vec![0u8; len.clamp(200, mesh_protocol::MAX_PAD)]));
    }
}

/// Keep cached blobs within budget.
async fn cache_pump(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        // Attachments requested in the last hour may still be on a stem route.
        let protect = mesh_object::now_ms().saturating_sub(3_600_000);
        let _ = inner.store.lock().unwrap().enforce_cache_budget(inner.config.cache_budget, protect);
    }
}

/// Keep trying to maintain a connection to every known and discovered peer.
async fn dialer(inner: Arc<Inner>) {
    loop {
        let stored = inner.store.lock().unwrap().peers().unwrap_or_default();
        let mut targets: Vec<(String, Option<Digest>)> = stored.into_iter().map(|p| (p.addr, p.identity)).collect();
        let now = Instant::now();
        {
            // Sightings go stale: radio addresses rotate and people walk away.
            let mut d = inner.discovered.lock().unwrap();
            d.retain(|_, (_, seen)| now.duration_since(*seen) < inner.config.discovered_ttl);
            targets.extend(d.iter().map(|(a, (i, _))| (a.clone(), *i)));
        }
        for (addr, expected) in targets {
            let waiting = inner.backoff.lock().unwrap().get(&addr).is_some_and(|(at, _)| now < *at);
            if !waiting && !already_connected(&inner, &addr) {
                spawn_dial(inner.clone(), addr, expected);
            }
        }
        tokio::time::sleep(inner.config.dial_interval).await;
    }
}

/// We already have a link to whoever answers at `addr`, over `addr`'s medium.
fn already_connected(inner: &Inner, addr: &str) -> bool {
    let Some(id) = inner.addr_identity.lock().unwrap().get(addr).copied() else { return false };
    let medium = resolve(inner, addr).2;
    inner.conns.lock().unwrap().values().any(|h| h.peer == id && h.medium == medium)
}

/// Dial `addr` unless a dial to it is already in flight. If `expected` is set,
/// the connection is refused unless the peer proves that identity (pinning).
fn spawn_dial(inner: Arc<Inner>, addr: String, expected: Option<Digest>) {
    if !inner.dialing.lock().unwrap().insert(addr.clone()) {
        return;
    }
    tokio::spawn(async move {
        let (transport, native, medium) = resolve(&inner, &addr);
        let failed = match transport.dial(&native).await {
            Ok(stream) => match run_connection(inner.clone(), stream, addr.clone(), false, expected, medium).await {
                Ok(()) => false,
                Err(e) => {
                    emit(&inner, Event::DialFailed { addr: addr.clone(), reason: format!("{e:#}") });
                    true
                }
            },
            Err(e) => {
                emit(&inner, Event::DialFailed { addr: addr.clone(), reason: e.to_string() });
                true
            }
        };
        {
            let mut b = inner.backoff.lock().unwrap();
            if failed {
                let base = inner.config.dial_interval;
                let next = b.get(&addr).map(|(_, d)| (*d * 2).min(Duration::from_secs(60))).unwrap_or(base);
                b.insert(addr.clone(), (Instant::now() + next, next));
            } else {
                b.remove(&addr);
            }
        }
        inner.dialing.lock().unwrap().remove(&addr);
    });
}
