//! `mesh` — reference command-line client (README §26).

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use mesh_crypto::{Digest, Keypair};
use mesh_discovery::{Invitation, LanDiscovery};
use mesh_node::{Config, Event, Node};
use mesh_object::blob::{guess_media_type, MAX_ATTACHMENT_SIZE};
use mesh_object::strip::{strip_metadata, Strip};
use mesh_object::{create, Attachment, Author, Content, VerifiedObject};
use mesh_store::{AttachmentState, BlobSource, Origin, Store, WantedAttachment};
use mesh_transport_tcp::{TcpTransport, DEFAULT_PORT};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "mesh", version, about = "SocialMesh: a decentralised, device-hosted social protocol")]
struct Cli {
    /// Node data directory (identity, objects, index).
    #[arg(long, global = true, env = "MESH_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new local identity and data directory.
    Init,
    /// Identity commands.
    Identity {
        #[command(subcommand)]
        cmd: IdentityCmd,
    },
    /// Run the node: accept peers, dial known peers, sync objects.
    Run {
        /// Address to listen on.
        #[arg(long, default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
        listen: String,
        /// Do not offer objects received from others onward (still shares your own).
        #[arg(long)]
        no_relay: bool,
        /// Listen for nearby nodes but never transmit discovery beacons.
        #[arg(long)]
        no_beacon: bool,
        /// Disable LAN discovery entirely (only dial saved peers).
        #[arg(long)]
        no_lan: bool,
        /// Cover traffic: decoy data sent per hour, in MiB, so your real
        /// anonymous sends don't stand out. 0 turns it off (weaker anonymity).
        #[arg(long, default_value_t = 16)]
        cover_mib_per_hour: u64,
        /// Most disk space for cached data from others, in MiB. Your own files don't count.
        #[arg(long, default_value_t = 2048)]
        cache_mib: u64,
        /// Keep one link key across runs instead of rotating it. Needed if you
        /// hand out `mesh invite` links; lets peers recognise this device.
        #[arg(long)]
        stable_link_key: bool,
    },
    /// Run a relay: connects, caches and passes data on for others. Holds no
    /// posting identity and never publishes. Suits an always-on spare device.
    Relay {
        /// Address to listen on.
        #[arg(long, default_value_t = format!("0.0.0.0:{DEFAULT_PORT}"))]
        listen: String,
        /// Listen for nearby nodes but never transmit discovery beacons.
        #[arg(long)]
        no_beacon: bool,
        /// Disable LAN discovery entirely (only dial saved peers).
        #[arg(long)]
        no_lan: bool,
        /// Cover traffic in MiB per hour. A relay usually has bandwidth to spare,
        /// and its cover helps hide everyone's anonymous sends around it.
        #[arg(long, default_value_t = 64)]
        cover_mib_per_hour: u64,
        /// Most disk space for cached data, in MiB.
        #[arg(long, default_value_t = 8192)]
        cache_mib: u64,
        /// Largest attachment fetched automatically to keep available, in MiB.
        #[arg(long, default_value_t = 16)]
        auto_fetch_mib: u64,
        /// Rotate the link key daily (and on network change) instead of keeping
        /// a stable one. Saved invitations to this relay stop working.
        #[arg(long)]
        rotate_link_key: bool,
    },
    /// Print a mesh1: invitation others can use to connect to you.
    Invite {
        /// Address(es) peers should dial, e.g. a public host:port. Defaults to this machine's LAN address.
        #[arg(long = "addr")]
        addrs: Vec<String>,
        /// Identity only, no addresses (for when you will be found by proximity).
        #[arg(long, conflicts_with = "addrs")]
        no_addr: bool,
    },
    /// Peer commands.
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Publish a post.
    Publish {
        text: String,
        /// Publish with no author (cannot be edited or withdrawn later).
        #[arg(long)]
        anonymous: bool,
        /// Treat the body as Markdown.
        #[arg(long)]
        markdown: bool,
        /// Attach a file (repeatable). Metadata is stripped, then the file is sealed.
        #[arg(long = "attach", value_name = "FILE")]
        attach: Vec<PathBuf>,
        /// Attach files whose metadata can't be stripped (PDF, video, ...) as they are.
        #[arg(long)]
        keep_metadata: bool,
    },
    /// Reply to an object.
    Reply {
        /// Object ID (full or short prefix).
        parent: String,
        text: String,
        /// Reply with a thread-scoped anonymous identity (stable within this thread only).
        #[arg(long, conflicts_with = "ownerless")]
        anonymous: bool,
        /// Reply with no author at all.
        #[arg(long)]
        ownerless: bool,
        /// Attach a file (repeatable). Metadata is stripped, then the file is sealed.
        #[arg(long = "attach", value_name = "FILE")]
        attach: Vec<PathBuf>,
        /// Attach files whose metadata can't be stripped (PDF, video, ...) as they are.
        #[arg(long)]
        keep_metadata: bool,
    },
    /// Queue an object's attachments for download. A running `mesh run` fetches
    /// them from connected peers.
    Fetch {
        /// Object ID (full or short prefix).
        id: String,
    },
    /// Save a downloaded attachment to a file, after checking it end to end.
    Save {
        /// Object ID (full or short prefix).
        id: String,
        /// Where to write the file.
        out: PathBuf,
        /// Which attachment, counting from 1.
        #[arg(long, default_value_t = 1)]
        index: usize,
    },
    /// Show top-level posts, newest first.
    Feed {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Object commands.
    Object {
        #[command(subcommand)]
        cmd: ObjectCmd,
    },
    /// Write shareable objects to an offline bundle file.
    Export { file: PathBuf },
    /// Verify and import objects from an offline bundle file.
    Import { file: PathBuf },
}

#[derive(Subcommand)]
enum IdentityCmd {
    Show,
}

#[derive(Subcommand)]
enum PeerCmd {
    /// Connect to an address, verify the identity behind it, and remember it.
    Add { addr: String },
    List,
    Remove { addr: String },
}

#[derive(Subcommand)]
enum ObjectCmd {
    /// Show an object and its replies.
    Show { id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let home = cli.home.unwrap_or_else(default_home);

    match cli.cmd {
        Cmd::Init => init(&home),
        Cmd::Identity { cmd: IdentityCmd::Show } => {
            let k = load_identity(&home)?;
            println!("Identity:   {}", k.public().id().short());
            println!("Full ID:    {}", k.public().id());
            println!("Public key: {}", hex::encode(k.public().to_bytes()));
            if link_key_path(&home).exists() {
                println!("Link key:   {} (stable; what peers see on links)", load_stable_link_key(&home)?.public().id().short());
            } else {
                println!("Link key:   rotating (a fresh one each run, daily and on network change)");
            }
            println!("Home:       {}", home.display());
            Ok(())
        }
        Cmd::Run { listen, no_relay, no_beacon, no_lan, cover_mib_per_hour, cache_mib, stable_link_key } => {
            let me = load_identity(&home)?.public().id();
            let config = Config {
                listen: Some(listen),
                relay: !no_relay,
                beacon: !no_beacon,
                cover_bytes_per_hour: cover_mib_per_hour * 1024 * 1024,
                cache_budget: cache_mib * 1024 * 1024,
                ..Config::default()
            };
            println!("Posting identity: {} (only appears in posts you sign, never on links)", me.short());
            run(&home, config, !no_lan, !stable_link_key, Some(me)).await
        }
        Cmd::Relay { listen, no_beacon, no_lan, cover_mib_per_hour, cache_mib, auto_fetch_mib, rotate_link_key } => {
            fs::create_dir_all(&home)?;
            let config = Config {
                listen: Some(listen),
                relay: true,
                beacon: !no_beacon,
                cover_bytes_per_hour: cover_mib_per_hour * 1024 * 1024,
                cache_budget: cache_mib * 1024 * 1024,
                auto_fetch_max: auto_fetch_mib * 1024 * 1024,
                ..Config::default()
            };
            println!("Relay mode: no posting identity; caches and passes data on for others.");
            println!("It can't read sealed files or tell real pieces from decoys.");
            run(&home, config, !no_lan, rotate_link_key, None).await
        }
        Cmd::Invite { addrs, no_addr } => {
            fs::create_dir_all(&home)?;
            let k = load_stable_link_key(&home)?;
            let addrs = if no_addr {
                vec![]
            } else if addrs.is_empty() {
                local_ipv4().map(|ip| vec![format!("{ip}:{DEFAULT_PORT}")]).unwrap_or_default()
            } else {
                addrs.iter().map(|a| normalise_addr(a)).collect()
            };
            println!("{}", Invitation { identity: k.public().id(), addrs });
            eprintln!("note: this names this device's link key, not your posting identity. It stays valid only while");
            eprintln!("      the node runs with a stable link key: `mesh run --stable-link-key` or `mesh relay`.");
            Ok(())
        }
        Cmd::Peer { cmd } => peer(&home, cmd).await,
        Cmd::Publish { text, anonymous, markdown, attach, keep_metadata } => {
            let k = load_identity(&home)?;
            let store = Store::open(&home)?;
            let mut c = attach_files(&store, Content::post(text), &attach, anonymous, keep_metadata)?;
            if markdown {
                c = c.markdown();
            }
            if anonymous {
                eprintln!("note: ownerless posts cannot be edited or withdrawn; copies may persist indefinitely.");
                let obj = create(coarsen(c), Author::Ownerless)?;
                save_anonymous(&store, &obj)
            } else {
                let obj = create(c, Author::Signed(&k))?;
                store.put(&obj, &Origin::Local)?;
                println!("Object:\n{}", obj.id().short());
                hint_if_not_running();
                Ok(())
            }
        }
        Cmd::Reply { parent, text, anonymous, ownerless, attach, keep_metadata } => {
            let k = load_identity(&home)?;
            let store = Store::open(&home)?;
            let parent_id = store.resolve(&parent)?;
            let root = thread_root(&store, parent_id)?;
            let c = attach_files(&store, Content::post(text).reply_to(parent_id), &attach, anonymous || ownerless, keep_metadata)?;
            if ownerless {
                save_anonymous(&store, &create(coarsen(c), Author::Ownerless)?)
            } else if anonymous {
                let tk = Keypair::derive_thread_key(&load_anon_secret(&home)?, &root);
                println!("Replying as Anonymous {}", tk.public().id().short());
                save_anonymous(&store, &create(coarsen(c), Author::Signed(&tk))?)
            } else {
                let obj = create(c, Author::Signed(&k))?;
                store.put(&obj, &Origin::Local)?;
                println!("Object:\n{}", obj.id().short());
                hint_if_not_running();
                Ok(())
            }
        }
        Cmd::Feed { limit } => {
            let store = Store::open(&home)?;
            let me = load_identity(&home)?.public().id();
            let roots = store.roots(limit)?;
            if roots.is_empty() {
                println!("(nothing yet: publish something, or `mesh peer add` and `mesh run`)");
            }
            for o in roots {
                print_object(&o, me, None, Some(&store));
                let n = store.count_children(&o.id())?;
                if n > 0 {
                    println!("  ↳ {n} repl{}  (mesh object show {})", if n == 1 { "y" } else { "ies" }, o.id().short());
                }
                println!();
            }
            Ok(())
        }
        Cmd::Object { cmd: ObjectCmd::Show { id } } => {
            let store = Store::open(&home)?;
            let me = load_identity(&home)?.public().id();
            let id = store.resolve(&id)?;
            let o = store.get(&id)?.context("object not stored locally")?;
            println!("id:      {}", o.id());
            println!("kind:    {}", o.object().kind);
            println!("type:    {}", o.object().body_type);
            println!("size:    {} bytes", o.bytes().len());
            if let Some(p) = o.object().parent {
                println!("parent:  {}", p.short());
            }
            if let Some(origin) = store.origin(&id)? {
                println!("origin:  {}", describe_origin(&origin));
            }
            println!();
            print_object(&o, me, None, Some(&store));
            print_replies(&store, &o.id(), me, 1)?;
            Ok(())
        }
        Cmd::Fetch { id } => {
            let store = Store::open(&home)?;
            let id = store.resolve(&id)?;
            let o = store.get(&id)?.context("object not stored locally")?;
            if o.object().attachments.is_empty() {
                println!("{} has no attachments.", id.short());
            }
            for (i, a) in o.object().attachments.iter().enumerate() {
                store.want_attachment(&a.into())?;
                let state = store.attachment_state(&a.object)?;
                println!("[{}] {} {}: {}", i + 1, a.media_type, human_size(a.size), describe_state(state.as_ref()));
            }
            if !o.object().attachments.is_empty() {
                eprintln!("(a running `mesh run` downloads queued attachments from connected peers)");
            }
            Ok(())
        }
        Cmd::Save { id, out, index } => {
            let store = Store::open(&home)?;
            let id = store.resolve(&id)?;
            let o = store.get(&id)?.context("object not stored locally")?;
            let a = index
                .checked_sub(1)
                .and_then(|i| o.object().attachments.get(i))
                .with_context(|| format!("{} has no attachment {index}", id.short()))?;
            if out.exists() {
                bail!("{} already exists; refusing to overwrite", out.display());
            }
            let tmp = out.with_extension("part");
            let result = (|| -> Result<()> {
                let mut f = std::io::BufWriter::new(fs::File::create(&tmp)?);
                store.write_attachment(&WantedAttachment::from(a), &mut f).map_err(|e| match e {
                    mesh_store::StoreError::Incomplete(_) => {
                        anyhow::anyhow!("not downloaded yet: run `mesh fetch {}` with `mesh run` going", id.short())
                    }
                    e => e.into(),
                })?;
                use std::io::Write;
                f.flush()?;
                Ok(())
            })();
            match result {
                Ok(()) => {
                    fs::rename(&tmp, &out)?;
                    println!("Saved {} ({}, {}): hash verified", out.display(), a.media_type, human_size(a.size));
                    eprintln!("note: treat attachments as untrusted files; open them with care.");
                    Ok(())
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(e)
                }
            }
        }
        Cmd::Export { file } => {
            let store = Store::open(&home)?;
            let mut f = std::io::BufWriter::new(fs::File::create(&file)?);
            let n = mesh_transport_file::export(&store, true, &mut f)?;
            println!("Exported {n} objects to {}", file.display());
            Ok(())
        }
        Cmd::Import { file } => {
            let store = Store::open(&home)?;
            let mut f = std::io::BufReader::new(fs::File::open(&file)?);
            let r = mesh_transport_file::import(&store, &mut f)?;
            println!("Imported {} new, {} already present, {} rejected", r.new, r.duplicate, r.rejected);
            Ok(())
        }
    }
}

fn default_home() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".socialmesh")
}

fn identity_path(home: &Path) -> PathBuf {
    home.join("identity.key")
}

fn init(home: &Path) -> Result<()> {
    let path = identity_path(home);
    if path.exists() {
        bail!("{} already exists; refusing to overwrite an identity", path.display());
    }
    fs::create_dir_all(home)?;
    let k = Keypair::generate();
    write_secret(&path, &k.seed())?;
    write_secret(&home.join("anonymous.key"), &mesh_crypto::random_bytes::<32>())?;
    Store::open(home)?;
    println!("Identity: {}", k.public().id().short());
    println!("Home:     {}", home.display());
    Ok(())
}

fn write_secret(path: &Path, secret: &[u8; 32]) -> Result<()> {
    fs::write(path, hex::encode(secret))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn read_secret(path: &Path) -> Result<[u8; 32]> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = hex::decode(text.trim()).context("secret file is not hex")?;
    raw.try_into().map_err(|_| anyhow::anyhow!("secret file has wrong length"))
}

fn load_identity(home: &Path) -> Result<Keypair> {
    let path = identity_path(home);
    if !path.exists() {
        bail!("no identity at {} — run `mesh init` first", home.display());
    }
    Ok(Keypair::from_seed(&read_secret(&path)?))
}

fn link_key_path(home: &Path) -> PathBuf {
    home.join("link.key")
}

/// The stable link key, created on first use. Never the posting identity.
fn load_stable_link_key(home: &Path) -> Result<Keypair> {
    let path = link_key_path(home);
    if !path.exists() {
        write_secret(&path, &mesh_crypto::random_bytes::<32>())?;
    }
    Ok(Keypair::from_seed(&read_secret(&path)?))
}

fn load_anon_secret(home: &Path) -> Result<[u8; 32]> {
    let path = home.join("anonymous.key");
    if !path.exists() {
        write_secret(&path, &mesh_crypto::random_bytes::<32>())?;
    }
    read_secret(&path)
}

/// Walk parents to find the thread root (bounded, and tolerant of missing ancestors).
fn thread_root(store: &Store, mut id: Digest) -> Result<Digest> {
    for _ in 0..256 {
        match store.get(&id)?.and_then(|o| o.object().parent) {
            Some(p) => id = p,
            None => break,
        }
    }
    Ok(id)
}

/// Run a node. `rotate`: fresh link key each run, daily and on network change;
/// otherwise the stable one in `link.key`. `me` is the posting identity, if
/// any, used only to label your own posts in the output.
async fn run(home: &Path, mut config: Config, lan: bool, rotate: bool, me: Option<Digest>) -> Result<()> {
    let (relay, beacon) = (config.relay, config.beacon);
    let store = Store::open(home)?;
    let me = me.unwrap_or_else(|| Digest::of(b"no posting identity"));
    let k = if rotate {
        config.link_key_rotation = Some(Duration::from_secs(24 * 3600));
        Keypair::generate()
    } else {
        load_stable_link_key(home)?
    };
    println!(
        "Link key: {} ({})",
        k.public().id().short(),
        if rotate { "rotates daily and on network change" } else { "stable" }
    );
    if config.cover_bytes_per_hour == 0 {
        println!("Cover traffic off: anonymous posts with files are easier to spot on your connection.");
    } else {
        println!("Cover traffic: about {} MiB/hour of decoy data.", config.cover_bytes_per_hour / (1024 * 1024));
    }
    let node = Node::new(k, store, Arc::new(TcpTransport), config);
    if lan {
        node.add_discovery(Arc::new(LanDiscovery::default()));
        println!(
            "LAN discovery on ({}).",
            if beacon { "announcing presence, no identity in beacons" } else { "listening only" }
        );
    }
    let mut events = node.subscribe();
    let running = node.start().await?;
    let mut net_check = tokio::time::interval(Duration::from_secs(30));
    let mut last_ip = local_ipv4();
    if !relay {
        println!("Relay disabled: only your own objects will be offered to peers.");
    }

    let mut failing: HashSet<String> = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = net_check.tick(), if rotate => {
                // New network: new key, so this device can't be followed between them.
                let ip = local_ipv4();
                if ip != last_ip {
                    last_ip = ip;
                    node.rotate_link_key();
                }
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => { eprintln!("({n} events skipped)"); continue }
                    Err(_) => break,
                };
                match ev {
                    Event::Listening(a) => println!("Listening on {a}..."),
                    Event::PeerConnected { peer, addr, inbound, medium } => {
                        failing.remove(&addr);
                        println!("Connected {} over {medium} ({} {addr})", peer.short(), if inbound { "from" } else { "to" });
                    }
                    Event::PeerDisconnected { peer, medium, reason, .. } => match reason {
                        Some(r) => println!("Disconnected {} over {medium}: {r}", peer.short()),
                        None => println!("Disconnected {} over {medium}", peer.short()),
                    },
                    Event::DialFailed { addr, reason } => {
                        if failing.insert(addr.clone()) {
                            println!("Cannot reach {addr}: {reason} (will keep retrying)");
                        }
                    }
                    Event::ObjectReceived { object, from } => {
                        println!("\nNEW OBJECT  {}  (via {})", object.id().short(), from.short());
                        print_object(&object, me, None, None);
                        println!("signature: {}", if object.object().author.is_some() { "valid" } else { "none (ownerless)" });
                        println!("hash: valid\n");
                    }
                    Event::Discovered { medium, addr } => println!("Found node via {medium} at {addr}"),
                    Event::ObjectRejected { from, reason } => println!("Rejected object from {}: {reason}", from.short()),
                    Event::LinkKeyRotated { id } => println!("New link key {}: reconnecting as a new node", id.short()),
                    Event::Fluffed(id) => println!("Anonymous object {} is now public (left stem routing at this node)", id.short()),
                    Event::AttachmentComplete { manifest } => println!("Attachment {} complete and verified", manifest.short()),
                    Event::AttachmentFailed { manifest, reason } => println!("Attachment {} failed: {reason}", manifest.short()),
                }
            }
        }
    }
    println!("Shutting down...");
    running.shutdown().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

async fn peer(home: &Path, cmd: PeerCmd) -> Result<()> {
    let store = Store::open(home)?;
    match cmd {
        PeerCmd::Add { addr } if addr.contains(mesh_discovery::invite::PREFIX) => {
            let k = Keypair::generate(); // probes never reveal a key, but don't touch ours
            let inv = Invitation::parse(&addr)?;
            if inv.addrs.is_empty() {
                println!("Invitation has no addresses. Run `mesh run`: the identity is pinned and will be");
                println!("accepted when found nearby, but proximity pinning is not persisted yet.");
                return Ok(());
            }
            let mut ok = false;
            for a in &inv.addrs {
                store.add_peer(a, Some(&inv.identity))?;
                match mesh_node::probe(&TcpTransport, &k, a, Duration::from_secs(10)).await {
                    Ok(pk) if pk.id() == inv.identity => {
                        println!("Connected {} at {a} (identity matches invitation)", pk.id().short());
                        ok = true;
                    }
                    Ok(pk) => {
                        store.remove_peer(a)?;
                        println!("REFUSED {a}: it proved identity {}, invitation is for {}", pk.id().short(), inv.identity.short());
                    }
                    Err(e) => println!("Could not reach {a} now ({e:#}); saved, `mesh run` will keep trying."),
                }
            }
            if ok {
                println!("Saved. `mesh run` will keep it in sync.");
            }
        }
        PeerCmd::Add { addr } => {
            let k = Keypair::generate();
            let addr = normalise_addr(&addr);
            match mesh_node::probe(&TcpTransport, &k, &addr, Duration::from_secs(10)).await {
                Ok(pk) => {
                    store.add_peer(&addr, Some(&pk.id()))?;
                    println!("Connected {}", pk.id().short());
                    println!("Saved peer {addr}. `mesh run` will keep it in sync.");
                }
                Err(e) => {
                    store.add_peer(&addr, None)?;
                    println!("Could not connect now ({e:#}).");
                    println!("Saved peer {addr} anyway; `mesh run` will keep trying.");
                }
            }
        }
        PeerCmd::List => {
            let peers = store.peers()?;
            if peers.is_empty() {
                println!("(no peers — add one with `mesh peer add <host:port>`)");
            }
            for p in peers {
                let id = p.identity.map(|d| d.short()).unwrap_or_else(|| "????????".into());
                let seen = p
                    .last_seen_ms
                    .map(|t| format!("last seen {}", ago(t)))
                    .unwrap_or_else(|| "never connected".into());
                println!("{id}  {:<24} {seen}", p.addr);
            }
        }
        PeerCmd::Remove { addr } => {
            let addr = normalise_addr(&addr);
            if store.remove_peer(&addr)? {
                println!("Removed {addr}");
            } else {
                println!("No peer {addr}");
            }
        }
    }
    Ok(())
}

fn normalise_addr(a: &str) -> String {
    let a = a.trim();
    let a = a.strip_prefix("tcp://").unwrap_or(a);
    if a.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        a.to_string()
    } else {
        format!("{a}:{DEFAULT_PORT}")
    }
}

/// This machine's primary LAN IPv4 address. Uses a routing-table lookup only:
/// connecting a UDP socket sends no packets.
fn local_ipv4() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET-1, never actually contacted
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

fn author_label(o: &VerifiedObject, me: Digest) -> String {
    match o.object().author {
        None => "Ownerless".into(),
        Some(a) if a.id() == me => format!("{} (you)", a.id().short()),
        Some(a) => a.id().short(),
    }
}

fn print_object(o: &VerifiedObject, me: Digest, indent: Option<usize>, store: Option<&Store>) {
    let pad = "  ".repeat(indent.unwrap_or(0));
    println!("{pad}{}  ·  {}  ·  {}", author_label(o, me), o.id().short(), when(o.object().created_ms));
    for line in o.object().body.lines() {
        println!("{pad}  {line}");
    }
    for (i, a) in o.object().attachments.iter().enumerate() {
        let state = store.map(|s| describe_state(s.attachment_state(&a.object).ok().flatten().as_ref()));
        let state = state.map(|s| format!(", {s}")).unwrap_or_default();
        println!("{pad}  [attachment {}: {}, {}{state}]", i + 1, a.media_type, human_size(a.size));
    }
}

fn print_replies(store: &Store, id: &Digest, me: Digest, depth: usize) -> Result<()> {
    if depth > 32 {
        return Ok(());
    }
    for c in store.children(id)? {
        println!();
        print_object(&c, me, Some(depth), Some(store));
        print_replies(store, &c.id(), me, depth + 1)?;
    }
    Ok(())
}

fn describe_origin(o: &Origin) -> String {
    match o {
        Origin::Local => "authored here".into(),
        Origin::Peer(p) => format!("received from peer {}", p.short()),
        Origin::Import => "imported from bundle".into(),
        Origin::Stem => "anonymity routing (authoring device not recorded)".into(),
    }
}

fn when(ms: u64) -> String {
    ago(ms)
}

fn ago(ms: u64) -> String {
    let now = mesh_object::now_ms();
    if ms > now {
        return "in the future".into();
    }
    let s = (now - ms) / 1000;
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// Round an anonymous object's timestamp down to the minute, so a precise
/// creation time can't be lined up with when traffic left this device.
fn coarsen(mut c: Content) -> Content {
    c.created_ms -= c.created_ms % 60_000;
    c
}

/// Hand an anonymous object to stem routing. It is stored exactly like an
/// object relayed for someone else, and is not announced from this device.
fn save_anonymous(store: &Store, obj: &VerifiedObject) -> Result<()> {
    store.put_stem(obj, true)?;
    println!("Object:\n{}", obj.id().short());
    eprintln!("(queued for anonymity routing: a running `mesh run` passes it to a relay peer,");
    eprintln!(" and it becomes public from somewhere along the route, not from this device)");
    Ok(())
}

/// Seal files into the store and attach them to `c`.
///
/// Anonymous attachments are stored as plain cache, exactly as a relay would
/// hold them, and are limited in size because every relay on the anonymity
/// route carries the whole file.
fn attach_files(store: &Store, mut c: Content, files: &[PathBuf], anonymous: bool, keep_metadata: bool) -> Result<Content> {
    let limit = if anonymous { mesh_node::Config::default().max_anonymous_attachments } else { MAX_ATTACHMENT_SIZE };
    let mut total = 0;
    for path in files {
        let len = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.len();
        total += len;
        if total > limit {
            let what = if anonymous { "anonymous attachments" } else { "an attachment" };
            bail!("{} would exceed the limit for {what} ({})", path.display(), human_size(limit));
        }
        let data = fs::read(path)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let media_type = guess_media_type(&name);
        let data = clean_metadata(&name, media_type, data, keep_metadata)?;
        let source = if anonymous { BlobSource::Cache } else { BlobSource::Own };
        let w = store.add_file(&data, source)?;
        c = c.attach(Attachment { object: w.manifest, key: w.key, media_type: media_type.into(), size: w.size });
        println!("Attached {name} ({media_type}, {}, sealed)", human_size(w.size));
    }
    Ok(c)
}

/// Strip identifying metadata, or refuse a file we can't clean unless the
/// user explicitly accepts the risk.
fn clean_metadata(name: &str, media_type: &str, data: Vec<u8>, keep_metadata: bool) -> Result<Vec<u8>> {
    match strip_metadata(media_type, &data) {
        Ok(Strip::Cleaned { data, removed }) => {
            if removed.is_empty() {
                println!("{name}: no metadata found");
            }
            for r in &removed {
                println!("{name}: removed {r}");
            }
            if media_type == "image/jpeg" && removed.iter().any(|r| r.starts_with("EXIF")) {
                eprintln!("note: {name} lost its rotation tag with the EXIF data; if it shows sideways, rotate and re-save it.");
            }
            Ok(data)
        }
        Ok(Strip::NothingToStrip) => Ok(data),
        Ok(Strip::Unsupported) if keep_metadata => {
            eprintln!("warning: {name} ({media_type}) is attached as-is. It may contain hidden metadata");
            eprintln!("         (author name, device, GPS location, edit history) that identifies you.");
            Ok(data)
        }
        Ok(Strip::Unsupported) => bail!(
            "{name}: can't remove metadata from {media_type} files. They often hold the author's name, device \
             or location. Export a clean copy (e.g. print to PDF, or convert a photo to JPEG/PNG), \
             or pass --keep-metadata to attach it as-is."
        ),
        Err(e) if keep_metadata => {
            eprintln!("warning: {name} couldn't be parsed as {media_type} ({e}); attached as-is.");
            Ok(data)
        }
        Err(e) => bail!("{name}: couldn't parse as {media_type} to remove metadata ({e}). Pass --keep-metadata to attach it as-is."),
    }
}

fn describe_state(s: Option<&AttachmentState>) -> String {
    match s {
        None => "not downloaded (mesh fetch)".into(),
        Some(AttachmentState::Wanted) => "downloading".into(),
        Some(AttachmentState::Complete) => "stored".into(),
        Some(AttachmentState::Failed(r)) => format!("failed: {r}"),
    }
}

fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
}

fn hint_if_not_running() {
    eprintln!("(a running `mesh run` for this home will announce it to peers)");
}
