//! Local storage (README §23).
//!
//! ```text
//! <home>/
//! ├── node.db          SQLite index, peer table, attachment state
//! ├── objects/
//! │   ├── a9/...        object bytes, named by full digest hex
//! │   └── ...
//! └── blobs/            sealed attachment pieces (uniform 64 KiB, opaque), same layout
//! ```
//!
//! Object bytes are written to disk before the index row, so an indexed object
//! always has its bytes present. Bytes are re-verified on every read: the disk
//! is not trusted any more than the network is.

use mesh_crypto::Digest;
use mesh_object::blob::{open_chunk, open_manifest, seal, Manifest, BLOB_SIZE};
use mesh_object::{Attachment, VerifiedObject};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("stored object {0} failed verification: {1}")]
    Corrupt(String, String),
    #[error("no object matches `{0}`")]
    NotFound(String),
    #[error("`{0}` is ambiguous; use more characters")]
    Ambiguous(String),
    #[error("attachment {0} is not complete")]
    Incomplete(String),
    #[error("invalid attachment: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Where an object entered this node from. Governs propagation (README §12, §17).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// Authored on this node.
    Local,
    /// Received from a connected peer (identity ID).
    Peer(Digest),
    /// Imported from an offline bundle.
    Import,
    /// Arrived through anonymity ("stem") routing, or authored here anonymously.
    /// The two cases are deliberately stored identically: the disk does not
    /// record who wrote an anonymous object (see docs/ORIGIN-PRIVACY.md).
    Stem,
}

impl Origin {
    fn to_db(&self) -> String {
        match self {
            Origin::Local => "local".into(),
            Origin::Peer(id) => format!("peer:{}", id.to_hex()),
            Origin::Import => "import".into(),
            Origin::Stem => "stem".into(),
        }
    }
    fn from_db(s: &str) -> Origin {
        match s {
            "local" => Origin::Local,
            "stem" => Origin::Stem,
            s => s
                .strip_prefix("peer:")
                .and_then(|h| Digest::from_hex(h).ok())
                .map(Origin::Peer)
                .unwrap_or(Origin::Import),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub seq: i64,
    pub id: Digest,
    pub origin: Origin,
    pub received_ms: u64,
    /// Still in the stem phase: must not be offered or served to anyone.
    pub embargoed: bool,
}

/// Local state of an attachment (by manifest ID).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentState {
    /// Queued for fetching; the node works on it while running.
    Wanted,
    /// Every chunk is here and the whole file matched its hash.
    Complete,
    /// Can never complete (e.g. the author published an inconsistent manifest).
    Failed(String),
}

/// Who a blob is kept for. Deliberately coarse: the store never records which
/// peer a blob came from, or whether this node created it for an anonymous post.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobSource {
    /// Part of a file attached to a *signed* post made here. Always served.
    Own,
    /// Everything else: fetched, pushed along a stem, or attached to an
    /// anonymous post made here. Served only when relaying.
    Cache,
}

/// An attachment the node should hold: enough to fetch and open it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WantedAttachment {
    pub manifest: Digest,
    pub key: [u8; 32],
    pub size: u64,
}

impl From<&Attachment> for WantedAttachment {
    fn from(a: &Attachment) -> Self {
        WantedAttachment { manifest: a.object, key: a.key, size: a.size }
    }
}

#[derive(Clone, Debug)]
pub struct PeerRecord {
    pub addr: String,
    pub identity: Option<Digest>,
    pub last_seen_ms: Option<u64>,
}

pub struct Store {
    root: PathBuf,
    db: Connection,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("objects"))?;
        fs::create_dir_all(root.join("blobs"))?;
        let db = Connection::open(root.join("node.db"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS objects (
                seq          INTEGER PRIMARY KEY AUTOINCREMENT,
                id           TEXT NOT NULL UNIQUE,
                kind         TEXT NOT NULL,
                author       TEXT,
                parent       TEXT,
                created_ms   INTEGER NOT NULL,
                received_ms  INTEGER NOT NULL,
                origin       TEXT NOT NULL,
                size         INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS objects_parent ON objects(parent);
            CREATE INDEX IF NOT EXISTS objects_created ON objects(created_ms);
            CREATE TABLE IF NOT EXISTS stem_outbox (
                id  TEXT PRIMARY KEY
            );
            CREATE TABLE IF NOT EXISTS blobs (
                id           TEXT PRIMARY KEY,
                size         INTEGER NOT NULL,
                origin       TEXT NOT NULL,
                received_ms  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS attachments (
                manifest      TEXT PRIMARY KEY,
                key           BLOB NOT NULL,
                size          INTEGER NOT NULL,
                state         TEXT NOT NULL,
                reason        TEXT,
                requested_ms  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS peers (
                addr          TEXT PRIMARY KEY,
                identity      TEXT,
                added_ms      INTEGER NOT NULL,
                last_seen_ms  INTEGER
            );
            "#,
        )?;
        // v0.1 migration: embargo column added for stem routing.
        let has_embargo: bool = db
            .prepare("SELECT 1 FROM pragma_table_info('objects') WHERE name = 'embargo_ms'")?
            .exists([])?;
        if !has_embargo {
            db.execute_batch("ALTER TABLE objects ADD COLUMN embargo_ms INTEGER")?;
        }
        Ok(Store { root, db })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, id: &Digest) -> PathBuf {
        let hex = id.to_hex();
        // Skip the algorithm byte so the fan-out spreads across 256 directories.
        self.root.join("objects").join(&hex[2..4]).join(hex)
    }

    /// Retain a verified object. Returns `true` if it was new.
    pub fn put(&self, obj: &VerifiedObject, origin: &Origin) -> Result<bool> {
        self.insert(obj, origin, None)
    }

    /// Retain an object in the stem phase: embargoed (never offered or served)
    /// until the node fluffs it. With `outbox`, the node will also push it into
    /// stem routing itself; used for objects authored here.
    ///
    /// Its attachments are queued for holding too. Author and relay take this
    /// same path, so the disk looks the same either way.
    pub fn put_stem(&self, obj: &VerifiedObject, outbox: bool) -> Result<bool> {
        let new = self.insert(obj, &Origin::Stem, Some(i64::MAX))?;
        if new {
            for a in &obj.object().attachments {
                self.want_attachment(&a.into())?;
            }
        }
        if new && outbox {
            self.db.execute("INSERT OR IGNORE INTO stem_outbox (id) VALUES (?1)", [obj.id().to_hex()])?;
        }
        Ok(new)
    }

    fn insert(&self, obj: &VerifiedObject, origin: &Origin, embargo_ms: Option<i64>) -> Result<bool> {
        if self.has(&obj.id())? {
            return Ok(false);
        }
        let path = self.object_path(&obj.id());
        fs::create_dir_all(path.parent().expect("object path has parent"))?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, obj.bytes())?;
        fs::rename(&tmp, &path)?;

        let o = obj.object();
        let inserted = self.db.execute(
            "INSERT OR IGNORE INTO objects (id, kind, author, parent, created_ms, received_ms, origin, size, embargo_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                obj.id().to_hex(),
                o.kind,
                o.author.map(|a| a.id().to_hex()),
                o.parent.map(|p| p.to_hex()),
                o.created_ms as i64,
                mesh_object::now_ms() as i64,
                origin.to_db(),
                obj.bytes().len() as i64,
                embargo_ms,
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn has(&self, id: &Digest) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT 1 FROM objects WHERE id = ?1", [id.to_hex()], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn get(&self, id: &Digest) -> Result<Option<VerifiedObject>> {
        if !self.has(id)? {
            return Ok(None);
        }
        let bytes = fs::read(self.object_path(id))?;
        let obj = VerifiedObject::decode(&bytes).map_err(|e| StoreError::Corrupt(id.to_hex(), e.to_string()))?;
        if obj.id() != *id {
            return Err(StoreError::Corrupt(id.to_hex(), "content hash mismatch".into()));
        }
        Ok(Some(obj))
    }

    pub fn origin(&self, id: &Digest) -> Result<Option<Origin>> {
        Ok(self
            .db
            .query_row("SELECT origin FROM objects WHERE id = ?1", [id.to_hex()], |r| r.get::<_, String>(0))
            .optional()?
            .map(|s| Origin::from_db(&s)))
    }

    /// Whether this node is willing to offer an object to others.
    ///
    /// Embargoed objects are never shareable. Local and stem objects are always
    /// shareable once public; everything else only when relaying is enabled.
    pub fn is_shareable(origin: &Origin, embargoed: bool, relay_enabled: bool) -> bool {
        !embargoed && (relay_enabled || matches!(origin, Origin::Local | Origin::Stem))
    }

    /// `is_shareable` for a stored object; `false` if absent.
    pub fn shareable(&self, id: &Digest, relay_enabled: bool) -> Result<bool> {
        let row = self
            .db
            .query_row(
                "SELECT origin, embargo_ms IS NOT NULL FROM objects WHERE id = ?1",
                [id.to_hex()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .optional()?;
        Ok(row.is_some_and(|(o, e)| Self::is_shareable(&Origin::from_db(&o), e, relay_enabled)))
    }

    pub fn is_embargoed(&self, id: &Digest) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT embargo_ms IS NOT NULL FROM objects WHERE id = ?1", [id.to_hex()], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    /// Set when an embargoed object should be fluffed if nobody else has.
    pub fn set_embargo(&self, id: &Digest, until_ms: u64) -> Result<()> {
        self.db.execute(
            "UPDATE objects SET embargo_ms = ?2 WHERE id = ?1 AND embargo_ms IS NOT NULL",
            params![id.to_hex(), until_ms as i64],
        )?;
        Ok(())
    }

    /// Make an embargoed object public. Returns `true` if it was embargoed.
    pub fn clear_embargo(&self, id: &Digest) -> Result<bool> {
        Ok(self.db.execute(
            "UPDATE objects SET embargo_ms = NULL WHERE id = ?1 AND embargo_ms IS NOT NULL",
            [id.to_hex()],
        )? > 0)
    }

    /// Lift an embargo without fanfare: the object moves to the end of the
    /// index so it is announced on the normal schedule, like a fresh arrival.
    /// Returns `true` if it was embargoed.
    pub fn make_public_quietly(&self, id: &Digest) -> Result<bool> {
        if !self.is_embargoed(id)? {
            return Ok(false);
        }
        self.db.execute(
            "UPDATE objects SET embargo_ms = NULL, seq = (SELECT MAX(seq) + 1 FROM objects) WHERE id = ?1",
            [id.to_hex()],
        )?;
        Ok(true)
    }

    /// Embargoed objects whose timer has run out.
    pub fn expired_embargoes(&self, now_ms: u64) -> Result<Vec<Digest>> {
        self.digests(
            "SELECT id FROM objects WHERE embargo_ms IS NOT NULL AND embargo_ms <= ?1",
            params![now_ms as i64],
        )
    }

    /// Locally authored objects waiting to enter stem routing.
    pub fn stem_outbox(&self) -> Result<Vec<Digest>> {
        self.digests("SELECT id FROM stem_outbox", [])
    }

    /// Put an object back in the outbox (its push to a relay failed part way).
    pub fn stem_outbox_add(&self, id: &Digest) -> Result<()> {
        self.db.execute("INSERT OR IGNORE INTO stem_outbox (id) VALUES (?1)", [id.to_hex()])?;
        Ok(())
    }

    /// Remove from the outbox. Called as soon as the object has been pushed,
    /// so the record linking this device to the object is short-lived.
    pub fn stem_outbox_remove(&self, id: &Digest) -> Result<()> {
        self.db.execute("DELETE FROM stem_outbox WHERE id = ?1", [id.to_hex()])?;
        Ok(())
    }

    fn digests(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Digest>> {
        let mut st = self.db.prepare(sql)?;
        let ids: Vec<String> = st.query_map(p, |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|h| Digest::from_hex(&h).map_err(|e| StoreError::Corrupt(h.clone(), e.to_string())))
            .collect()
    }

    /// IDs this node is willing to announce to peers.
    pub fn shareable_ids(&self, relay_enabled: bool) -> Result<Vec<Digest>> {
        Ok(self.entries_since(0)?
            .into_iter()
            .filter(|e| Self::is_shareable(&e.origin, e.embargoed, relay_enabled))
            .map(|e| e.id)
            .collect())
    }

    pub fn max_seq(&self) -> Result<i64> {
        Ok(self.db.query_row("SELECT COALESCE(MAX(seq), 0) FROM objects", [], |r| r.get(0))?)
    }

    /// Objects indexed after `seq`, oldest first.
    pub fn entries_since(&self, seq: i64) -> Result<Vec<Entry>> {
        let mut st = self
            .db
            .prepare("SELECT seq, id, origin, received_ms, embargo_ms IS NOT NULL FROM objects WHERE seq > ?1 ORDER BY seq")?;
        let rows = st.query_map([seq], row_to_entry)?;
        collect_entries(rows)
    }

    /// Top-level objects (no parent), newest first.
    pub fn roots(&self, limit: usize) -> Result<Vec<VerifiedObject>> {
        self.load_ids(
            "SELECT id FROM objects WHERE parent IS NULL ORDER BY created_ms DESC LIMIT ?1",
            params![limit as i64],
        )
    }

    /// Everything, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<VerifiedObject>> {
        self.load_ids("SELECT id FROM objects ORDER BY created_ms DESC LIMIT ?1", params![limit as i64])
    }

    /// Direct replies to `parent`, oldest first.
    pub fn children(&self, parent: &Digest) -> Result<Vec<VerifiedObject>> {
        self.load_ids(
            "SELECT id FROM objects WHERE parent = ?1 ORDER BY created_ms",
            params![parent.to_hex()],
        )
    }

    pub fn count_children(&self, parent: &Digest) -> Result<u64> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM objects WHERE parent = ?1",
            [parent.to_hex()],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    fn load_ids(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<VerifiedObject>> {
        let mut st = self.db.prepare(sql)?;
        let ids: Vec<String> = st.query_map(p, |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for h in ids {
            let id = Digest::from_hex(&h).map_err(|e| StoreError::Corrupt(h.clone(), e.to_string()))?;
            if let Some(o) = self.get(&id)? {
                out.push(o);
            }
        }
        Ok(out)
    }

    /// Resolve a full hex ID, or an unambiguous prefix of the short form shown in the UI.
    pub fn resolve(&self, text: &str) -> Result<Digest> {
        let t = text.trim().to_ascii_lowercase();
        if let Ok(d) = Digest::from_hex(&t) {
            return Ok(d);
        }
        if t.is_empty() || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::NotFound(text.into()));
        }
        // Short forms omit the algorithm byte; match both spellings.
        let mut st = self.db.prepare("SELECT id FROM objects WHERE substr(id, 3) LIKE ?1 OR id LIKE ?1 LIMIT 2")?;
        let hits: Vec<String> = st
            .query_map([format!("{t}%")], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        match hits.as_slice() {
            [one] => Digest::from_hex(one).map_err(|e| StoreError::Corrupt(one.clone(), e.to_string())),
            [] => Err(StoreError::NotFound(text.into())),
            _ => Err(StoreError::Ambiguous(text.into())),
        }
    }

    // ---- blobs (sealed attachment pieces) -----------------------------------

    fn blob_path(&self, id: &Digest) -> PathBuf {
        let hex = id.to_hex();
        self.root.join("blobs").join(&hex[2..4]).join(hex)
    }

    /// Retain a blob. Its ID is its hash, so it can't be stored under a wrong
    /// name. `Own` upgrades an existing `Cache` entry; never the reverse.
    pub fn put_blob(&self, data: &[u8], source: BlobSource) -> Result<Digest> {
        if data.len() != BLOB_SIZE {
            return Err(StoreError::Invalid(format!("blob is {} bytes, expected {BLOB_SIZE}", data.len())));
        }
        let id = Digest::of(data);
        let src = match source {
            BlobSource::Own => "own",
            BlobSource::Cache => "cache",
        };
        if !self.has_blob(&id)? {
            let path = self.blob_path(&id);
            fs::create_dir_all(path.parent().expect("blob path has parent"))?;
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, data)?;
            fs::rename(&tmp, &path)?;
        }
        // No timestamp and no peer: the row says only that we hold it.
        self.db.execute(
            "INSERT INTO blobs (id, size, origin, received_ms) VALUES (?1, ?2, ?3, 0)
             ON CONFLICT(id) DO UPDATE SET origin = 'own' WHERE excluded.origin = 'own'",
            params![id.to_hex(), data.len() as i64, src],
        )?;
        Ok(id)
    }

    pub fn has_blob(&self, id: &Digest) -> Result<bool> {
        Ok(self.db.query_row("SELECT 1 FROM blobs WHERE id = ?1", [id.to_hex()], |_| Ok(())).optional()?.is_some())
    }

    /// Read a blob, re-verifying its hash.
    pub fn get_blob(&self, id: &Digest) -> Result<Option<Vec<u8>>> {
        if !self.has_blob(id)? {
            return Ok(None);
        }
        let data = fs::read(self.blob_path(id))?;
        if !id.matches(&data) {
            return Err(StoreError::Corrupt(id.to_hex(), "blob hash mismatch".into()));
        }
        Ok(Some(data))
    }

    pub fn blob_source(&self, id: &Digest) -> Result<Option<BlobSource>> {
        let o: Option<String> =
            self.db.query_row("SELECT origin FROM blobs WHERE id = ?1", [id.to_hex()], |r| r.get(0)).optional()?;
        Ok(o.map(|s| if s == "own" { BlobSource::Own } else { BlobSource::Cache }))
    }

    /// Whether we will send a blob to peers: own files always, cache only when relaying.
    pub fn blob_shareable(&self, id: &Digest, relay_enabled: bool) -> Result<bool> {
        Ok(match self.blob_source(id)? {
            Some(BlobSource::Own) => true,
            Some(BlobSource::Cache) => relay_enabled,
            None => false,
        })
    }

    fn delete_blob(&self, id: &Digest) -> Result<()> {
        match fs::remove_file(self.blob_path(id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.db.execute("DELETE FROM blobs WHERE id = ?1", [id.to_hex()])?;
        Ok(())
    }

    /// Bytes held as cache (everything except blobs of our own signed posts).
    pub fn cache_bytes(&self) -> Result<u64> {
        Ok(self.db.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs WHERE origin = 'cache'", [], |r| {
            r.get::<_, i64>(0)
        })? as u64)
    }

    /// Bring cached blobs under `budget` bytes. Returns bytes evicted.
    ///
    /// 1. Orphans first: cached blobs no known attachment refers to. These are
    ///    cover traffic and pieces whose post never arrived. Oldest first.
    /// 2. Then whole cached attachments, least recently requested first,
    ///    skipping any requested after `protect_after_ms` (e.g. a file still
    ///    travelling a stem). Their record is dropped, so they show as not
    ///    downloaded.
    ///
    /// Blobs of our own signed posts are never evicted.
    pub fn enforce_cache_budget(&self, budget: u64, protect_after_ms: u64) -> Result<u64> {
        let mut used = self.cache_bytes()?;
        if used <= budget {
            return Ok(0);
        }
        let start = used;

        // Which blobs belong to an attachment we know about.
        let mut st = self.db.prepare("SELECT manifest, key, requested_ms FROM attachments ORDER BY requested_ms")?;
        let rows: Vec<(String, Vec<u8>, i64)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        drop(st);
        let mut groups: Vec<(Digest, Vec<Digest>, u64)> = Vec::new();
        let mut referenced = std::collections::HashSet::new();
        for (h, k, requested) in rows {
            let Ok(m) = Digest::from_hex(&h) else { continue };
            let Ok(key) = <[u8; 32]>::try_from(k) else { continue };
            let mut ids = vec![m];
            if let Ok(Some(man)) = self.manifest(&m, &key) {
                ids.extend(man.chunks);
            }
            referenced.extend(ids.iter().copied());
            groups.push((m, ids, requested as u64));
        }

        // 1. Orphans, oldest first.
        let orphans: Vec<Digest> = self
            .digests("SELECT id FROM blobs WHERE origin = 'cache' ORDER BY rowid", [])?
            .into_iter()
            .filter(|id| !referenced.contains(id))
            .collect();
        for id in orphans {
            if used <= budget {
                break;
            }
            self.delete_blob(&id)?;
            used = used.saturating_sub(BLOB_SIZE as u64);
        }

        // 2. Whole cached attachments, least recently requested first.
        for (manifest, ids, requested) in groups {
            if used <= budget {
                break;
            }
            if requested > protect_after_ms {
                continue;
            }
            let mut own = false;
            for id in &ids {
                own |= self.blob_source(id)? == Some(BlobSource::Own);
            }
            if own {
                continue;
            }
            for id in &ids {
                if self.has_blob(id)? {
                    self.delete_blob(id)?;
                    used = used.saturating_sub(BLOB_SIZE as u64);
                }
            }
            self.db.execute("DELETE FROM attachments WHERE manifest = ?1", [manifest.to_hex()])?;
        }
        Ok(start - used)
    }

    /// Seal a local file into the store under a fresh key. Returns what the
    /// post needs (`manifest`, `key`, `size`).
    ///
    /// `Own` is for signed posts: the file is marked complete and always served.
    /// `Cache` is for anonymous posts: the blobs are stored exactly as if pushed
    /// here by a relay, and nothing records that this node created them. The
    /// attachment is queued when the post enters stem routing (`put_stem`).
    pub fn add_file(&self, data: &[u8], source: BlobSource) -> Result<WantedAttachment> {
        let sealed = seal(data).map_err(|e| StoreError::Invalid(e.to_string()))?;
        for b in &sealed.blobs {
            self.put_blob(b, source)?;
        }
        let att = WantedAttachment { manifest: sealed.manifest_id, key: sealed.key, size: sealed.size };
        if source == BlobSource::Own {
            self.want_attachment(&att)?;
            self.set_attachment_state(&att.manifest, &AttachmentState::Complete)?;
        }
        Ok(att)
    }

    /// Open a stored manifest with its key.
    pub fn manifest(&self, id: &Digest, key: &[u8; 32]) -> Result<Option<Manifest>> {
        match self.get_blob(id)? {
            None => Ok(None),
            Some(b) => open_manifest(&b, key).map(Some).map_err(|e| StoreError::Invalid(e.to_string())),
        }
    }

    /// Chunk IDs of `manifest` not yet stored.
    pub fn missing_chunks(&self, manifest: &Manifest) -> Result<Vec<Digest>> {
        let mut out = Vec::new();
        for c in &manifest.chunks {
            if !self.has_blob(c)? {
                out.push(*c);
            }
        }
        Ok(out)
    }

    /// Every blob of a complete attachment, manifest first.
    pub fn attachment_blobs(&self, att: &WantedAttachment) -> Result<Vec<Digest>> {
        let m = self.manifest(&att.manifest, &att.key)?.ok_or_else(|| StoreError::Incomplete(att.manifest.short()))?;
        let mut ids = vec![att.manifest];
        ids.extend(m.chunks);
        Ok(ids)
    }

    /// Queue an attachment for holding (no-op if already known).
    pub fn want_attachment(&self, att: &WantedAttachment) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO attachments (manifest, key, size, state, requested_ms) VALUES (?1, ?2, ?3, 'wanted', ?4)",
            params![att.manifest.to_hex(), &att.key[..], att.size as i64, mesh_object::now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn attachment_state(&self, manifest: &Digest) -> Result<Option<AttachmentState>> {
        let row: Option<(String, Option<String>)> = self
            .db
            .query_row("SELECT state, reason FROM attachments WHERE manifest = ?1", [manifest.to_hex()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        Ok(row.map(|(s, reason)| match s.as_str() {
            "complete" => AttachmentState::Complete,
            "failed" => AttachmentState::Failed(reason.unwrap_or_default()),
            _ => AttachmentState::Wanted,
        }))
    }

    pub fn set_attachment_state(&self, manifest: &Digest, state: &AttachmentState) -> Result<()> {
        let (s, reason) = match state {
            AttachmentState::Wanted => ("wanted", None),
            AttachmentState::Complete => ("complete", None),
            AttachmentState::Failed(r) => ("failed", Some(r.as_str())),
        };
        self.db.execute(
            "UPDATE attachments SET state = ?2, reason = ?3 WHERE manifest = ?1",
            params![manifest.to_hex(), s, reason],
        )?;
        Ok(())
    }

    /// Attachments queued for fetching.
    pub fn wanted_attachments(&self) -> Result<Vec<WantedAttachment>> {
        let mut st = self
            .db
            .prepare("SELECT manifest, key, size FROM attachments WHERE state = 'wanted' ORDER BY requested_ms")?;
        let rows: Vec<(String, Vec<u8>, i64)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(h, k, n)| {
                let manifest = Digest::from_hex(&h).map_err(|e| StoreError::Corrupt(h.clone(), e.to_string()))?;
                let key = k.try_into().map_err(|_| StoreError::Corrupt(h.clone(), "bad key length".into()))?;
                Ok(WantedAttachment { manifest, key, size: n as u64 })
            })
            .collect()
    }

    /// Decrypt and reassemble an attachment into `out`, checking every chunk
    /// and the whole-file hash. On error `out` may hold partial data: write to
    /// a temporary file and keep it only on success.
    pub fn write_attachment(&self, att: &WantedAttachment, out: &mut impl Write) -> Result<Manifest> {
        let m = self.manifest(&att.manifest, &att.key)?.ok_or_else(|| StoreError::Incomplete(att.manifest.short()))?;
        if m.size != att.size {
            return Err(StoreError::Invalid(format!("manifest says {} bytes, post says {}", m.size, att.size)));
        }
        let mut h = Digest::hasher();
        for (i, c) in m.chunks.iter().enumerate() {
            let blob = self.get_blob(c)?.ok_or_else(|| StoreError::Incomplete(att.manifest.short()))?;
            let data = open_chunk(&blob, &att.key, &m, i).map_err(|e| StoreError::Invalid(format!("chunk {i}: {e}")))?;
            h.update(&data);
            out.write_all(&data)?;
        }
        if h.finalize() != m.content {
            return Err(StoreError::Invalid("reassembled file does not match its hash".into()));
        }
        Ok(m)
    }

    /// Check a complete attachment without writing it anywhere.
    pub fn verify_attachment(&self, att: &WantedAttachment) -> Result<Manifest> {
        self.write_attachment(att, &mut std::io::sink())
    }

    // ---- peers ----------------------------------------------------------

    pub fn add_peer(&self, addr: &str, identity: Option<&Digest>) -> Result<()> {
        self.db.execute(
            "INSERT INTO peers (addr, identity, added_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(addr) DO UPDATE SET identity = COALESCE(excluded.identity, peers.identity)",
            params![addr, identity.map(|d| d.to_hex()), mesh_object::now_ms() as i64],
        )?;
        Ok(())
    }

    /// Record a successful connection. Does not pin the identity that
    /// answered: link keys may rotate, so only an explicit pin (an invitation,
    /// `add_peer` with an identity) is enforced.
    pub fn touch_peer(&self, addr: &str) -> Result<()> {
        self.db.execute(
            "UPDATE peers SET last_seen_ms = ?2 WHERE addr = ?1",
            params![addr, mesh_object::now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn remove_peer(&self, addr: &str) -> Result<bool> {
        Ok(self.db.execute("DELETE FROM peers WHERE addr = ?1", [addr])? > 0)
    }

    pub fn peers(&self) -> Result<Vec<PeerRecord>> {
        let mut st = self.db.prepare("SELECT addr, identity, last_seen_ms FROM peers ORDER BY added_ms")?;
        let rows = st.query_map([], |r| {
            Ok(PeerRecord {
                addr: r.get(0)?,
                identity: r.get::<_, Option<String>>(1)?.and_then(|h| Digest::from_hex(&h).ok()),
                last_seen_ms: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

type EntryRow = (i64, String, String, i64, bool);

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn collect_entries(rows: impl Iterator<Item = rusqlite::Result<EntryRow>>) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    for row in rows {
        let (seq, id, origin, received_ms, embargoed) = row?;
        let id = Digest::from_hex(&id).map_err(|e| StoreError::Corrupt(id.clone(), e.to_string()))?;
        out.push(Entry { seq, id, origin: Origin::from_db(&origin), received_ms: received_ms as u64, embargoed });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mesh_crypto::Keypair;
    use mesh_object::{create, Author, Content};

    #[test]
    fn put_get_and_threads() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let k = Keypair::generate();
        let root = create(Content::post("root"), Author::Signed(&k)).unwrap();
        let reply = create(Content::post("reply").reply_to(root.id()), Author::Ownerless).unwrap();

        assert!(s.put(&root, &Origin::Local).unwrap());
        assert!(!s.put(&root, &Origin::Local).unwrap(), "duplicate is not new");
        assert!(s.put(&reply, &Origin::Peer(k.public().id())).unwrap());

        assert_eq!(s.get(&root.id()).unwrap().unwrap().object().body, "root");
        assert_eq!(s.children(&root.id()).unwrap().len(), 1);
        assert_eq!(s.roots(10).unwrap().len(), 1);
        assert_eq!(s.resolve(&root.id().short()).unwrap(), root.id());
        assert_eq!(s.shareable_ids(false).unwrap(), vec![root.id()]);
        assert_eq!(s.shareable_ids(true).unwrap().len(), 2);
    }

    #[test]
    fn stem_objects_are_embargoed_and_unattributed() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let o = create(Content::post("anon"), Author::Ownerless).unwrap();
        assert!(s.put_stem(&o, true).unwrap());
        assert_eq!(s.origin(&o.id()).unwrap(), Some(Origin::Stem), "never recorded as Local");
        assert!(!s.shareable(&o.id(), true).unwrap(), "embargoed objects are never served");
        assert!(s.shareable_ids(true).unwrap().is_empty());
        assert_eq!(s.stem_outbox().unwrap(), vec![o.id()]);
        s.stem_outbox_remove(&o.id()).unwrap();
        assert!(s.stem_outbox().unwrap().is_empty());

        s.set_embargo(&o.id(), 1000).unwrap();
        assert!(s.expired_embargoes(999).unwrap().is_empty());
        assert_eq!(s.expired_embargoes(1000).unwrap(), vec![o.id()]);
        assert!(s.clear_embargo(&o.id()).unwrap());
        assert!(!s.clear_embargo(&o.id()).unwrap());
        assert!(s.shareable(&o.id(), false).unwrap(), "public stem objects are shareable even without relay");
    }

    #[test]
    fn attachments_roundtrip_and_resist_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let att = s.add_file(&data, BlobSource::Own).unwrap();
        assert_eq!(s.attachment_state(&att.manifest).unwrap(), Some(AttachmentState::Complete));
        let mut out = Vec::new();
        s.write_attachment(&att, &mut out).unwrap();
        assert_eq!(out, data);
        assert!(s.blob_shareable(&att.manifest, false).unwrap(), "own files are served without relay");
        let blobs = s.attachment_blobs(&att).unwrap();
        assert_eq!(blobs.len(), 5);

        // A second store fetching it: cached blobs are served only when relaying.
        let dir2 = tempfile::tempdir().unwrap();
        let t = Store::open(dir2.path()).unwrap();
        t.want_attachment(&att).unwrap();
        assert_eq!(t.wanted_attachments().unwrap(), vec![att.clone()]);
        t.put_blob(&s.get_blob(&att.manifest).unwrap().unwrap(), BlobSource::Cache).unwrap();
        let m = t.manifest(&att.manifest, &att.key).unwrap().unwrap();
        assert_eq!(t.missing_chunks(&m).unwrap().len(), 4);
        assert!(matches!(t.verify_attachment(&att), Err(StoreError::Incomplete(_))));
        for c in &m.chunks {
            t.put_blob(&s.get_blob(c).unwrap().unwrap(), BlobSource::Cache).unwrap();
        }
        t.verify_attachment(&att).unwrap();
        assert!(!t.blob_shareable(&att.manifest, false).unwrap());
        assert!(t.blob_shareable(&att.manifest, true).unwrap());
        let wrong = WantedAttachment { key: [9; 32], ..att.clone() };
        assert!(t.verify_attachment(&wrong).is_err(), "wrong key");

        // Disk tampering is caught on read.
        fs::write(t.blob_path(&m.chunks[1]), b"evil").unwrap();
        assert!(t.verify_attachment(&att).is_err());
    }

    #[test]
    fn anonymous_attachment_leaves_no_authorship_trace() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let att = s.add_file(b"leaked memo", BlobSource::Cache).unwrap();
        assert_eq!(s.blob_source(&att.manifest).unwrap(), Some(BlobSource::Cache));
        assert_eq!(s.attachment_state(&att.manifest).unwrap(), None, "no record until it enters routing");
        let a = Attachment { object: att.manifest, key: att.key, media_type: "text/plain".into(), size: att.size };
        let o = mesh_object::create(Content::post("anon").attach(a), Author::Ownerless).unwrap();
        s.put_stem(&o, true).unwrap();
        // Same row a relay would write on receiving the stem.
        assert_eq!(s.attachment_state(&att.manifest).unwrap(), Some(AttachmentState::Wanted));
        let received: i64 =
            s.db.query_row("SELECT MAX(received_ms) FROM blobs", [], |r| r.get(0)).unwrap();
        assert_eq!(received, 0, "no arrival times recorded for blobs");
    }

    #[test]
    fn cache_budget_evicts_orphans_then_old_attachments_never_own() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let mine = s.add_file(&vec![1u8; 200_000], BlobSource::Own).unwrap(); // 5 blobs, never evicted
        let cached = s.add_file(&vec![2u8; 200_000], BlobSource::Cache).unwrap(); // 5 blobs
        s.want_attachment(&cached).unwrap();
        let junk: Vec<Digest> =
            (0..4u8).map(|i| s.put_blob(&vec![i + 10; BLOB_SIZE], BlobSource::Cache).unwrap()).collect();
        let blob = BLOB_SIZE as u64;
        assert_eq!(s.cache_bytes().unwrap(), 9 * blob);

        // Budget for exactly the attachment's 5 blobs: the 4 orphans go, it stays.
        s.enforce_cache_budget(5 * blob, 0).unwrap();
        assert!(junk.iter().all(|id| !s.has_blob(id).unwrap()));
        s.verify_attachment(&cached).unwrap();

        // A recently requested attachment is protected...
        s.enforce_cache_budget(0, 0).unwrap();
        s.verify_attachment(&cached).unwrap();
        // ...until it isn't.
        s.enforce_cache_budget(0, u64::MAX).unwrap();
        assert!(s.attachment_state(&cached.manifest).unwrap().is_none());
        assert_eq!(s.cache_bytes().unwrap(), 0);
        s.verify_attachment(&mine).unwrap();
    }

    #[test]
    fn detects_corruption_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let o = create(Content::post("x"), Author::Ownerless).unwrap();
        s.put(&o, &Origin::Local).unwrap();
        fs::write(s.object_path(&o.id()), b"garbage").unwrap();
        assert!(matches!(s.get(&o.id()), Err(StoreError::Corrupt(..))));
    }
}
