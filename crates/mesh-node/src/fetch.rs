//! Attachment fetching (README §11, Milestone 4).
//!
//! Attachments are sealed blobs (`mesh_object::blob`). The post carries the
//! manifest ID and key. For each queued attachment the scheduler:
//!
//! 1. asks a peer for the manifest blob and opens it with the key, then
//! 2. spreads the missing chunk blobs across *all* connected peers, at most
//!    [`WINDOW`] outstanding per peer, in any order.
//!
//! Every blob is named by hashing what actually arrived, so a wrong or
//! corrupted blob can't be accepted, only discarded as unsolicited (the sender
//! takes a strike and is benched). A peer that answers BLOB_MISSING, or doesn't
//! answer in time, is skipped for that blob for a while. When the last chunk
//! lands, every chunk is decrypted and the whole file checked against the
//! manifest's content hash.
//!
//! Blobs that arrive some other way (pushed along a stem with STASH) count too:
//! each tick drops whatever is already stored from the to-do list.

use crate::{emit, Event, Inner};
use mesh_crypto::Digest;
use mesh_object::blob::{open_manifest, Manifest};
use mesh_protocol::Message;
use mesh_store::{AttachmentState, BlobSource, Store, WantedAttachment};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Outstanding blob requests per peer.
pub const WINDOW: usize = 8;
/// Forget a request after this long and ask someone else.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// After a peer says it doesn't have a blob, don't ask it again for this long.
/// It may be fetching the same attachment itself.
pub const REFUSAL_BACKOFF: Duration = Duration::from_secs(3);
/// A peer that sends a blob we didn't ask for is not asked for anything for
/// this long, and its outstanding requests go to other peers immediately.
pub const JUNK_PENALTY: Duration = Duration::from_secs(60);
/// Attachments worked on at once; the rest wait in the queue.
const MAX_ACTIVE: usize = 8;
const TICK: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct Scheduler {
    fetches: HashMap<Digest, Fetch>,
    /// Blob → (connection asked, when).
    inflight: HashMap<Digest, (u64, Instant)>,
    /// Connection → not asked again before this time (it sent junk).
    penalized: HashMap<u64, Instant>,
    /// Blobs accepted per peer this run. In memory only, never on disk.
    pub(crate) received_from: HashMap<Digest, usize>,
}

struct Fetch {
    want: WantedAttachment,
    manifest: Option<Manifest>,
    /// Chunks still missing (empty until the manifest is known).
    need: HashSet<Digest>,
    /// Blob → connections that recently said they don't have it.
    refused: HashMap<Digest, HashMap<u64, Instant>>,
}

impl Fetch {
    fn new(want: WantedAttachment) -> Self {
        Fetch { want, manifest: None, need: HashSet::new(), refused: HashMap::new() }
    }

    /// Adopt the manifest if it is stored now. `Err` if it can never work.
    fn load_manifest(&mut self, store: &Store) -> Result<(), String> {
        if self.manifest.is_some() {
            return Ok(());
        }
        let Some(blob) = store.get_blob(&self.want.manifest).map_err(|e| e.to_string())? else { return Ok(()) };
        // The blob hashed to the ID in the signed post, so a bad manifest is
        // exactly what the author published: it can never complete.
        let m = open_manifest(&blob, &self.want.key).map_err(|e| format!("invalid manifest: {e}"))?;
        if m.size != self.want.size {
            return Err(format!("manifest says {} bytes, post says {}", m.size, self.want.size));
        }
        self.need = store.missing_chunks(&m).unwrap_or_default().into_iter().collect();
        self.manifest = Some(m);
        Ok(())
    }
}

impl Scheduler {
    /// The attachment a blob belongs to, if we are fetching it.
    fn owner(&self, blob: &Digest) -> Option<Digest> {
        self.fetches.iter().find(|(m, f)| *m == blob || f.need.contains(blob)).map(|(m, _)| *m)
    }

    pub(crate) fn active(&self) -> usize {
        self.fetches.len()
    }
}

/// Handle a BLOB from `peer`. Returns `false` if it isn't part of anything we
/// are fetching (the caller strikes the peer). A late reply to a request that
/// timed out is still accepted: it was verified by its hash either way.
pub(crate) fn on_blob(inner: &Inner, peer: Digest, data: &[u8]) -> bool {
    let id = Digest::of(data);
    let mut sched = inner.blobs.lock().unwrap();
    let Some(owner) = sched.owner(&id) else { return false };
    sched.inflight.remove(&id);
    let store = inner.store.lock().unwrap();
    if store.put_blob(data, BlobSource::Cache).is_err() {
        return false; // wrong size: not a valid blob
    }
    *sched.received_from.entry(peer).or_default() += 1;
    let f = sched.fetches.get_mut(&owner).expect("owner exists");
    f.need.remove(&id);
    let status = f.load_manifest(&store);
    let done = f.manifest.is_some() && f.need.is_empty();
    match status {
        Err(reason) => finish(inner, &mut sched, &store, owner, Err(reason)),
        Ok(()) if done => {
            let want = sched.fetches[&owner].want.clone();
            let verdict = store.verify_attachment(&want).map(|_| ()).map_err(|e| e.to_string());
            finish(inner, &mut sched, &store, owner, verdict);
        }
        Ok(()) => {}
    }
    true
}

/// Connection `conn` sent a blob we didn't ask for (junk or garbage). Stop
/// relying on it: everything it was asked for is reassigned right away.
pub(crate) fn on_junk(inner: &Inner, conn: u64) {
    let mut sched = inner.blobs.lock().unwrap();
    sched.penalized.insert(conn, Instant::now() + JUNK_PENALTY);
    sched.inflight.retain(|_, (c, _)| *c != conn);
}

/// Handle BLOB_MISSING from connection `conn`.
pub(crate) fn on_missing(inner: &Inner, conn: u64, ids: &[Digest]) {
    let mut sched = inner.blobs.lock().unwrap();
    let now = Instant::now();
    for id in ids {
        if sched.inflight.get(id).is_some_and(|(c, _)| *c == conn) {
            sched.inflight.remove(id);
        }
        if let Some(owner) = sched.owner(id) {
            let f = sched.fetches.get_mut(&owner).expect("owner exists");
            f.refused.entry(*id).or_default().insert(conn, now);
        }
    }
}

fn finish(inner: &Inner, sched: &mut Scheduler, store: &Store, manifest: Digest, result: Result<(), String>) {
    if let Some(f) = sched.fetches.remove(&manifest) {
        for c in f.need {
            sched.inflight.remove(&c);
        }
    }
    sched.inflight.remove(&manifest);
    match result {
        Ok(()) => {
            let _ = store.set_attachment_state(&manifest, &AttachmentState::Complete);
            emit(inner, Event::AttachmentComplete { manifest });
        }
        Err(reason) => {
            let _ = store.set_attachment_state(&manifest, &AttachmentState::Failed(reason.clone()));
            emit(inner, Event::AttachmentFailed { manifest, reason });
        }
    }
}

/// Background task: pick up queued attachments and keep requests flowing.
pub(crate) async fn pump(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(TICK);
    loop {
        tick.tick().await;
        schedule(&inner);
    }
}

fn schedule(inner: &Inner) {
    let peers: Vec<(u64, mpsc::Sender<Message>)> =
        inner.conns.lock().unwrap().iter().map(|(id, h)| (*id, h.tx.clone())).collect();
    let mut sched = inner.blobs.lock().unwrap();
    let now = Instant::now();

    // 1. Load queued attachments; account for blobs that arrived some other way.
    {
        let store = inner.store.lock().unwrap();
        for want in store.wanted_attachments().unwrap_or_default() {
            if sched.fetches.contains_key(&want.manifest) {
                continue;
            }
            if sched.fetches.len() >= MAX_ACTIVE {
                break;
            }
            sched.fetches.insert(want.manifest, Fetch::new(want));
        }
        let ids: Vec<Digest> = sched.fetches.keys().copied().collect();
        for m in ids {
            let f = sched.fetches.get_mut(&m).expect("listed");
            if let Err(reason) = f.load_manifest(&store) {
                finish(inner, &mut sched, &store, m, Err(reason));
                continue;
            }
            f.need.retain(|c| !store.has_blob(c).unwrap_or(false));
            for r in f.refused.values_mut() {
                r.retain(|_, t| now.duration_since(*t) < REFUSAL_BACKOFF);
            }
            if f.manifest.is_some() && f.need.is_empty() {
                let want = f.want.clone();
                let verdict = store.verify_attachment(&want).map(|_| ()).map_err(|e| e.to_string());
                finish(inner, &mut sched, &store, m, verdict);
            }
        }
    }

    // 2. Requests that went unanswered: count as a refusal, ask someone else.
    let live: HashSet<u64> = peers.iter().map(|(c, _)| *c).collect();
    let expired: Vec<(Digest, u64)> = sched
        .inflight
        .iter()
        .filter(|(_, (c, t))| now.duration_since(*t) >= REQUEST_TIMEOUT || !live.contains(c))
        .map(|(id, (c, _))| (*id, *c))
        .collect();
    for (id, conn) in expired {
        sched.inflight.remove(&id);
        if let Some(owner) = sched.owner(&id) {
            let f = sched.fetches.get_mut(&owner).expect("owner exists");
            f.refused.entry(id).or_default().insert(conn, now);
        }
    }
    sched.penalized.retain(|c, until| *until > now && live.contains(c));
    if peers.is_empty() {
        return;
    }

    // 3. Hand out requests, least-loaded peer first.
    let mut load: HashMap<u64, usize> =
        peers.iter().filter(|(c, _)| !sched.penalized.contains_key(c)).map(|(c, _)| (*c, 0)).collect();
    for (c, _) in sched.inflight.values() {
        if let Some(n) = load.get_mut(c) {
            *n += 1;
        }
    }
    let capacity = load.len() * WINDOW;
    let mut todo: Vec<(Digest, HashSet<u64>)> = Vec::new();
    for (m, f) in &sched.fetches {
        let items: Vec<Digest> = if f.manifest.is_none() { vec![*m] } else { f.need.iter().copied().collect() };
        for id in items {
            if todo.len() >= capacity {
                break;
            }
            if !sched.inflight.contains_key(&id) {
                let refused = f.refused.get(&id).map(|r| r.keys().copied().collect()).unwrap_or_default();
                todo.push((id, refused));
            }
        }
    }
    let mut batches: HashMap<u64, Vec<Digest>> = HashMap::new();
    for (id, refused) in todo {
        let pick = load
            .iter()
            .filter(|(c, n)| **n < WINDOW && !refused.contains(c))
            .min_by_key(|(c, n)| (**n, **c))
            .map(|(c, _)| *c);
        if let Some(c) = pick {
            *load.get_mut(&c).expect("known") += 1;
            sched.inflight.insert(id, (c, now));
            batches.entry(c).or_default().push(id);
        }
    }
    for (c, ids) in batches {
        let tx = &peers.iter().find(|(p, _)| *p == c).expect("known").1;
        if tx.try_send(Message::BlobWant(ids.clone())).is_err() {
            for id in ids {
                sched.inflight.remove(&id);
            }
        }
    }
}
