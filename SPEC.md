# SocialMesh Wire Specification v0.1

This document pins down every choice that affects interoperability. A second
implementation that follows it byte-for-byte must produce identical object IDs
and be able to talk to the reference implementation. The design rationale lives
in README.md; this file is normative.

Conventions: all integers are **big-endian**. `str` / `bytes` are a `u32`
length followed by that many bytes (`str` must be UTF-8). `opt<T>` is a byte
`0x00` (absent) or `0x01` followed by `T`; any other flag value is invalid.
Decoders MUST reject trailing bytes, and any field over its limit.

## 1. Primitives

| Purpose | Choice |
|---|---|
| Signatures | Ed25519 (RFC 8032), verified with *strict* verification |
| Hash | BLAKE3-256 |
| Randomness | OS CSPRNG |

### 1.1 Digest

```
digest = u8 alg || 32 bytes        (33 bytes)
alg 0x01 = BLAKE3-256. Any other value MUST be rejected in v0.1.
```

Text form: lowercase hex of all 33 bytes (66 chars). The UI may show a short
form (first 4 digest bytes after `alg`, uppercase); short forms are never used
on the wire.

### 1.2 Identity ID

```
identity_id = 0x01 || BLAKE3.derive_key("socialmesh v1 identity id", public_key[32])
```

### 1.3 Signing input (domain separation)

```
signing_input(context, payload) = UTF8(context) || 0x00 || payload
```

### 1.4 Thread-scoped anonymous key (README §8)

```
seed = BLAKE3.derive_key("socialmesh v1 thread anonymous key",
                         anonymous_secret[32] || root_thread_id[33])
keypair = Ed25519 keypair from seed
```

`anonymous_secret` is a 32-byte random value that never leaves the device.
`root_thread_id` is the ID of the topmost known ancestor.

## 2. Limits

| Item | Limit |
|---|---|
| Encoded object | 262 144 bytes |
| `kind` | 1–64 bytes, `[a-z0-9._-]` |
| media type | 1–127 printable ASCII, must contain `/` |
| `body` | 65 536 bytes |
| attachments | 64 |
| Wire frame | 263 168 bytes |
| IDs per HAVE/WANT/BLOB_WANT/BLOB_MISSING | 1 024 |
| Attachment | 1 – 129 926 160 bytes (1 983 chunks) |
| Blob (sealed manifest or chunk) | exactly 65 536 bytes |
| Anonymous object's attachments, total | 16 MiB (node policy, SHOULD) |

## 3. Object encoding

```
unsigned =
    "SMSH"                    4 bytes magic
    u8   format_version       = 1
    str  kind                 e.g. "post"
    u64  created_ms           Unix time, milliseconds (author's claim; not trusted)
    opt< [32] public_key >    author; absent = ownerless
    opt< digest >             parent
    str  body_type            "text/plain" | "text/markdown" | other media type
    str  body
    u16  attachment_count
    attachment_count × ( digest manifest || [32] key || str media_type || u64 size )

object   = unsigned || opt< [64] signature >

signature = Ed25519.sign(author_key, signing_input("socialmesh v1 object", unsigned))
ObjectID  = 0x01 || BLAKE3(object)
```

Rules:

- `signature` is present **iff** `author` is present.
- A receiver MUST verify the signature, MUST reject non-canonical encodings
  (re-encoding the decoded object must reproduce the input exactly), and MUST
  verify `ObjectID` against the ID it requested.
- Ed25519 signing is deterministic, so identical content + key + timestamp
  produce an identical ID.

## 4. Attachments: sealed blobs

A file is encrypted when it is attached, under a fresh random 32-byte `key`
that appears only in the post. The result is a set of **blobs**, every one
exactly 65 536 bytes of ciphertext, named by its digest:

```
AEAD      = ChaCha20-Poly1305 (RFC 8439), associated data "socialmesh v1 blob"
nonce(k,i)= u8 k || 0x000000 || u64 i         (12 bytes)
P         = 65 520                             (plaintext per blob)

chunk i      : file bytes [i·P, min((i+1)·P, size)), zero-padded to P
chunk blob i = AEAD.seal(key, nonce(0x02, i), chunk i)
chunk ID i   = 0x01 || BLAKE3(chunk blob i)

manifest plaintext =
    "SMMF"              magic
    u8   version        = 1
    u64  size           file size, 1 … 129 926 160
    digest content      BLAKE3 of the complete plaintext file
    u32  n              = ceil(size / P)   (≤ 1 983)
    n × digest          chunk IDs in file order
    zero padding to P bytes
manifest blob = AEAD.seal(key, nonce(0x01, 0), manifest plaintext)
manifest ID   = 0x01 || BLAKE3(manifest blob)
```

The attachment entry in the post is `manifest ID || key || media_type ||
size`.

Rules:

- `key` MUST be random (OS CSPRNG) and MUST NOT be reused. It MUST NOT be
  derived from the file: content-derived keys let anyone holding a known file
  test whether a node has cached it.
- Every blob is the same size and opaque, so a node holding blobs cannot
  tell a manifest from a chunk, which blobs belong together, or what any of
  them contain, without the post.
- A blob is named only by hashing what arrived. A node accepts a blob only if
  its hash is one it is fetching (or it is a STASH, §5.5).
- On open, the receiver MUST reject: a blob that fails AEAD authentication, a
  manifest with wrong magic/version, `size` out of range or different from the
  post's `size`, `n ≠ ceil(size / P)`, or non-zero padding in the manifest or
  in any chunk.
- After all chunks are present the receiver MUST check that the decrypted
  file hashes to `content`.
- File names are not part of the protocol.

## 5. Peer protocol

### 5.0 Secure channel

Every connection, on every stream transport, starts with a Noise handshake.
Nothing travels in the clear after the first two handshake messages.

```
Noise protocol: Noise_NN_25519_ChaChaPoly_BLAKE2s
Prologue:       "socialmesh v1"
Initiator:      the side that dialled

handshake message = u16 length || noise_message      (payloads empty)
  -> e
  <- e, ee
```

`binding` = the Noise handshake hash (32 bytes) once the handshake completes.
It is unique to the connection and is signed in HELLO (§5.3).

After the handshake, each direction is a sequence of records:

```
record    = u16 ciphertext_len || ciphertext          (ciphertext_len ≥ 18)
plaintext = u16 data_len || data || padding
```

- Encryption uses the Noise transport cipher with an implicit nonce counter
  per direction, starting at 0 and incremented per record.
- Senders pad each plaintext with zeros to a multiple of 256 bytes, capped at
  65 519. Receivers MUST ignore padding contents.
- `data` from successive records is concatenated into a byte stream that
  carries the frames of §5.1. Record boundaries carry no meaning.
- A record that fails to decrypt, or whose `data_len` exceeds the record, MUST
  close the connection.

Noise `NN` authenticates no one by itself. Identity comes from HELLO, which is
bound to `binding`.

**HELLO carries a link key, not a posting identity.** Anyone who connects
(anyone in radio range, for a phone) learns the key in HELLO, so it MUST NOT
be a key the user signs posts with. A node SHOULD use a separate link key and
MAY rotate it: on rotation it closes every link and reconnects under the new
key, so no peer sees the old and new keys side by side. Nodes others reach by
invitation (relays, home servers) keep a stable link key. Posting identities
appear only inside signed objects.

### 5.0.1 Link liveness

- A node SHOULD send PAD on a link that has sent nothing for a while
  (reference: 10 s).
- A node SHOULD close a link that has received nothing for several of those
  periods (reference: 35 s). Peers leaving radio range rarely say goodbye.
- A new link to a peer over a medium that already has one **replaces** the
  old link, except in a dial race (§5.3.1), so a returning peer always gets
  through even if the old link isn't known to be dead yet.

### 5.1 Framing

```
frame = u32 length || message      (length ≤ 263 168, checked before allocation)
```

### 5.2 Messages

| Tag | Name | Fields |
|---|---|---|
| `0x01` | HELLO | `u16 protocol(=1)`, `u8 flags`, `[32] public_key`, `[64] signature` |
| `0x10` | HAVE | `u16 n`, `n × digest` |
| `0x11` | WANT | `u16 n`, `n × digest` |
| `0x12` | OBJECT | `bytes object` |
| `0x13` | STEM | `bytes object` |
| `0x14` | STASH | `bytes blob` |
| `0x20` | BLOB_WANT | `u16 n`, `n × digest` |
| `0x21` | BLOB | `bytes blob` |
| `0x22` | BLOB_MISSING | `u16 n`, `n × digest` |
| `0x30` | PAD | `bytes` (≤ 4 096), ignored by the receiver |

HELLO flags: bit `0x01` = accepts STEM (node relays for others). Other bits
are reserved and MUST be sent as zero.

Tag `0x02` is reserved (AUTH in early drafts) and MUST NOT be sent. Unknown
tags are a protocol error.

### 5.3 Session

1. Establish the secure channel (§5.0). All further messages travel inside it.
2. The **responder** sends HELLO first. Its signature is
   ```
   sign(signing_input("socialmesh v1 handshake",
        binding[32] || role u8 || protocol u16 || flags u8 || public_key[32]))
   ```
   with `role` = `0x02` for the responder and `0x01` for the initiator.
3. The initiator verifies the responder's HELLO. A bad signature, a different
   protocol version, our own key, or (if the initiator pinned an identity, e.g.
   from an invitation) a different identity closes the connection **before the
   initiator sends anything**, so it never reveals its identity to an impostor.
4. Otherwise the initiator sends its own HELLO, signed the same way, and the
   responder verifies it.

   A node MAY close after step 3 to learn a peer's identity without disclosing
   its own (the reference CLI does this in `mesh peer add`).
5. After the handshake each side sends HAVE for every object it is willing to
   share, then HAVE for newly retained objects as they arrive.
6. On HAVE, request unknown objects with WANT (bounded outstanding requests).
7. On WANT, reply with OBJECT for each requested object the node is willing to share.
8. OBJECT that was not requested on this connection is discarded.

A second HELLO is a protocol error.

### 5.3.1 Several links to one peer

Two nodes MAY be connected over several media at once (TCP, Bluetooth, Wi-Fi
Direct, ...). Each link is a complete, separately encrypted session (§5.0,
§5.3); nothing ties links together on the wire.

- Addresses name their medium as `medium:address` (e.g. `ble:…`). An address
  without a known prefix uses the node's default transport (TCP).
- A node keeps **at most one link per peer per medium**. If both ends dial
  each other over the same medium, both keep the link whose initiator has the
  smaller identity ID (byte order) and close the other. A dial race is two
  links from opposite ends within a few seconds (reference: 5 s). Otherwise
  the newer link replaces the older (§5.0.1).
- Requests are answered on the link they arrived on. A node SHOULD spread its
  own traffic across a peer's links: blob requests to the least-loaded link,
  STASH runs shared between links (each link takes the next piece when free
  and is paced on its own, §5.5), and control messages (HAVE, STEM, PAD) on a
  random link. HAVE for an object is sent on one link per peer, not all.

### 5.4 Propagation policy

A node always offers objects authored locally. It offers objects received from
peers or imported from bundles only when relaying is enabled. Offering an
object is transport, not endorsement (README §17).

### 5.4.1 Attachment transfer

Attachments are pulled on demand, never announced in HAVE.

- On BLOB_WANT, a node sends BLOB for each requested blob it holds and is
  willing to share, and lists the rest in one BLOB_MISSING. A node always
  shares blobs of files it attached to its own *signed* posts; all other
  blobs (fetched, stashed, or attached to its own anonymous posts) only when
  relaying. A node SHOULD answer BLOB_MISSING rather than delay when busy.
- A BLOB whose hash is not part of an attachment the receiver is fetching is
  unsolicited: it is discarded and counts against the sender.
- Requests are independent. A receiver MAY ask different peers for different
  blobs at once, in any order, and MAY ask again elsewhere after BLOB_MISSING
  or a timeout.
- Senders SHOULD send queued control messages (HAVE, WANT, BLOB_WANT,
  BLOB_MISSING, STEM without attachments) before queued OBJECT/BLOB/STASH data.
- A node SHOULD NOT record which peer a blob came from or when.

### 5.5 Private-origin routing (stem / fluff)

Normative rules; rationale in docs/ORIGIN-PRIVACY.md.

- **Private-origin objects** are NOT announced by their author. These are
  anonymous objects (ownerless, or signed with a thread-scoped key) and signed
  objects whose author asks for it. Signing means choosing to be known, so
  signed objects are normally announced directly; the private route only
  hides which *device* a signed post came from. (Reference client:
  `--private-route`.)
- The author SHOULD hold a new private-origin object for a random time before
  sending it (reference: 2–30 s).
- Routing: The author sends them as STEM to one successor
  that advertised `FLAG_ACCEPTS_STEM`. If no such peer exists, the author
  waits; it MUST NOT fall back to HAVE.
- A node holding an object in the stem phase ("embargoed") MUST NOT include it
  in HAVE and MUST NOT serve it in response to WANT.
- On receiving STEM for an unknown object, a relaying node verifies it, stores
  it embargoed, then with probability `p` (default 0.1) **fluffs** it (lifts the
  embargo and sends HAVE to all peers). Otherwise it forwards STEM to its
  successor, excluding the sender; with no successor, it fluffs.
- The author never fluffs at its own hop.
- STEM for an object already held ends the stem: if still embargoed, fluff.
- Each node keeps one successor per epoch (default 10 minutes).
- Each holder sets a random embargo timer (default 20–50 s) and fluffs on expiry.
- On seeing HAVE for an object it holds under embargo, a node lifts the
  embargo without an immediate announcement and announces it on its normal
  schedule.
- A node that does not relay MUST NOT set `FLAG_ACCEPTS_STEM`, and MAY treat
  STEM as a protocol violation.
- Private-origin objects SHOULD round `created_ms` down to a whole minute.
- Their attachments are stored by the author exactly like stashed ones
  (cache, no "own" marker), and the anonymous size limit applies.
- **Attachments ride the stem.** Before sending STEM for an object with
  attachments, a node hands every blob of those attachments to the successor
  as STASH, spread over all of its links to the successor (§5.3.1), then sends
  the STEM on one of them. Every hop on the route ends up holding the file.
  The author follows the same rule.
- Over several links the STEM can overtake pieces still queued on a slower
  one. A relay that receives a STEM before all of its pieces SHOULD wait while
  pieces keep arriving (reference: give up after 15 s with no new piece, or
  10 minutes in all).
- A node that does not hold every blob of a stem object's attachments (after
  that wait) MUST NOT forward it as STEM; it fluffs instead (upstream holders
  serve the rest).
- STASH is accepted only by relaying nodes, only as a 65 536-byte blob, and
  rate-limited per peer (default 1 024 per minute). It is stored like any
  other cached blob.
- The author stores the sealed blobs of its own anonymous attachments exactly
  as it would stashed ones (no "own" marker).
- STASH pieces SHOULD be paced: a random gap (default 20–120 ms) before each
  piece, the same for authors, relays and cover traffic.
- A node SHOULD start an object's fallback embargo timer only after it has
  queued the STEM, and SHOULD extend it by about four times the time it spent
  pushing the object's pieces (later hops take as long again). An author whose
  push fails MUST requeue the object for another route, not fluff it.

### 5.6 Cover traffic

**Single messages.** A STEM leaving its author is a message nothing on the
link triggered (a relay's STEM follows one it received). To hide that, a node
SHOULD send single decoy PADs, sized like posts (log-uniform 200–4 096
bytes), on the control queue of a random relaying peer at exponentially
distributed intervals (reference: 60 per hour, a few KB). Keepalive PADs
(§5.0.1) SHOULD be jittered so they don't form a steady beat.

**Runs.**

To hide real STASH runs from link observers, a node SHOULD send decoy runs to
relaying peers: STASH pieces of 65 536 random bytes (indistinguishable from
sealed blobs), paced as in §5.5, followed by one PAD (as a real run is
followed by a STEM). Run lengths SHOULD be log-uniform from 1 to the largest
real anonymous send (257 pieces by default), at exponentially distributed
intervals that average a configured rate (reference default: 16 MiB/hour).

Receivers can't tell decoys from real pieces and store them as cache. A node
SHOULD bound its cache and evict, in order: pieces no known attachment refers
to (oldest first), then whole cached attachments (least recently requested
first, skipping recent ones that may still be on a stem route). Blobs of the
node's own signed posts are never evicted.

## 6. Offline bundle

```
"SMBUNDLE" || u8 version || u32 count || count × ( u32 len || object )
            [ version 2: || u32 blob_count || blob_count × blob ]
```

Version 1 carries objects only (datagram media, §8). Version 2 adds the
blobs (§4) of the objects' attachments, so photos and site files (§10)
travel on a USB stick too. Each object is verified exactly as if it had
arrived over the network; an invalid entry is skipped, not fatal. Blobs are
named by their hash and held as cache, as if a peer had sent them.

## 7. Discovery

### 7.1 LAN beacon (UDP port 7843, broadcast)

```
"SMDISC" || u8 version(=1) || [16] instance || u16 port      (25 bytes)
```

`instance` is random per run and used only to ignore one's own beacons. The
beacon **does not contain an identity**. A sighting is a hint to dial
`source_ip:port`; identity is established by the handshake.

### 7.2 Invitation

```
mesh1:<identity-id hex>[@addr[,addr...]]
```

Parsers accept the string embedded in a longer URL (`https://…#mesh1:…`).
Addresses are hints; the identity is pinned during the handshake. The
identity in an invitation is the node's (stable) **link key** (§5.0), not
its owner's posting identity. Addresses stored without an explicit identity
are not pinned to whichever key first answers, because link keys may rotate.

## 8. Datagram mode (one-way and lossy media)

For media with no reliable two-way stream: sound, LoRa, QR sequences,
broadcasts. There is no handshake and no link encryption, so a sender MUST
only send objects that are already public (never embargoed ones).

### 8.1 Frames

The payload is an offline bundle (§6), erasure-coded with RaptorQ (RFC 6330)
as one source block with alignment 8. Every frame stands alone:

```
"SD" | version u8 = 1 | symbol_size/8 u8 | transfer [4] | payload_len u32
     | packet_id [4] (RFC 6330 payload ID) | symbol [symbol_size] | check [4]
```

- `transfer` = first 4 bytes of BLAKE3(payload). Frames group by it; the
  rebuilt payload MUST match it.
- `check` = first 4 bytes of BLAKE3 over the frame before it. A frame that
  fails is dropped (an erasure).
- `payload_len` ≤ 1 MiB.
- A sender emits the source symbols, then repair symbols, as many as the
  medium allows. A receiver can start at any frame and needs about as many
  intact frames as there are source symbols (a couple more at most).
- Receivers bound how many unfinished transfers they hold (reference: 16).
- Recovered bundles are imported with full object verification.

### 8.2 Sound modem

16-FSK, one 4-bit symbol per tone, tone *k* at 1 000 + 100·*k* Hz, sampled
at 8 kHz, continuous phase. Profiles: ROBUST, 160 samples per symbol
(200 bit/s); FAST, 80 (400 bit/s). On air:

```
preamble (16 symbols: 0 9 3 14 6 1 12 7 15 4 10 2 13 8 5 11)
| length u8, 3 times | frame bytes | 32 check bytes | 2 symbols of silence
```

Bytes go high nibble first. `length` is the frame's (1–223); receivers take
each nibble by majority of the three copies. The check bytes are
Reed–Solomon RS(255, 223) over GF(2^8), primitive polynomial 0x11d,
generator roots α^0 … α^31, systematic and shortened to the frame: they
repair up to 16 wrong bytes.

Receivers find preambles by the share of in-band tone energy on the expected
tone (noise ≈ 1/16), and read each symbol from its end, skipping the start
where the previous symbol's echo lands: the last 10 ms (1/spacing, the
shortest window that separates the tones) or the last three quarters,
whichever is shorter. Real rooms echo for well over 5 ms. Speakers and rooms are not flat (phones measured up to 23 dB apart
across the band), so receivers SHOULD judge each tone relative to how loud
it arrives when sent, not by raw power. Reference symbol size per frame:
64 bytes.

## 9. Bluetooth LE

A link over Bluetooth LE is an ordinary transport stream (§5): the secure
channel runs over it unchanged. Addresses are written `ble:<device>`, where
`<device>` is whatever the platform calls the remote radio.

### 9.1 Advert

Nodes that accept links advertise, connectable, the service UUID
`191e3b36-5476-45bf-82eb-a2c4070948e7` with 10 bytes of service data:

```
version u8 = 1 | flags u8 = 0 | psm u16 | token [6]
```

- `psm`: the L2CAP channel accepting links, or 0 if only the GATT pipe
  (§9.2) is offered.
- `token`: random. Replaced whenever the radio address changes.
- Receivers MUST ignore data with another version, and accept longer data
  (later fields are appended).
- The advert MUST NOT contain anything that identifies the node or its user,
  including the link key. Flags AD + service data fit a legacy 31-byte
  advert.
- A platform that can't put service data in an advert (iOS) advertises the
  UUID alone and serves the same 10 bytes in the readable characteristic
  `191e3b39-5476-45bf-82eb-a2c4070948e7`.
- Nodes SHOULD also put the bare service UUID in the scan response, so
  platforms that filter scans by UUID list find them.
- A listen-only node (`beacon` off) never advertises; it scans and connects.

### 9.2 Links

Preferred: an L2CAP connection-oriented channel on the advertised `psm`.
Otherwise, the **GATT pipe** inside the SocialMesh service:

- `191e3b37-…948e7` (RX): the central writes without response;
- `191e3b38-…948e7` (TX): the peripheral notifies.

The central opens a pipe by enabling notifications on TX, and MUST NOT
write to RX before that; writes from a central without a pipe are dropped.
Both directions carry the stream as consecutive byte chunks of at most
ATT_MTU − 3 bytes, in order. There is no framing; a disconnect, or the
central turning notifications off, ends the stream. Centrals SHOULD ask
for the largest ATT MTU.

A node that accepts links MUST offer the GATT pipe, even when it also
advertises a `psm`: some platforms (Windows) can't open L2CAP channels and
use the pipe whatever the advert says. Links on either are equal.

### 9.3 Who connects

When both ends advertise, the end with the numerically smaller token
connects. The other end connects anyway if no link appeared within 20 s of
first hearing the peer. A peer heard without a token is always connected to.
A node reports a device to itself at most every 30 s, and never while a link
to it is open. A device not heard for 60 s is not dialled.

### 9.4 Address rotation

Whenever the node replaces its link key (§5.0) the radio MUST switch to a
new random address and the advert MUST get a new token, so an observer
can't link the old key to the new one by address.

## 10. Sites

A site is a set of files (HTML, CSS, images, media) published as one object
and passed around like any other: over every medium, through relays, in
offline bundles. There is no server and no DNS. Its files are ordinary sealed
attachments (§4), so relays carry them without being able to read them.

### 10.1 Site object

An object (§3) with:

- `kind` = `site`;
- `body_type` = `application/vnd.socialmesh.site`;
- `body` = the site manifest (§10.2);
- one attachment per file, `media_type` set to the file's type;
- `parent` absent.

A site is either:

- **keyed**: signed by a **site key**, an Ed25519 keypair made for this site
  alone. Its address is the site key's identity ID (§1.2). Only the key's
  holder can publish new versions. The site key SHOULD NOT be the author's
  posting identity: a site is not linked to who wrote it unless they say so;
- **ownerless**: unsigned. Its address is its object ID. It can never be
  updated; a changed site is a different site.

Text form of an address: `site:` followed by the 66 hex characters of the
identity ID or object ID. The UI may show a short form (§1.1).

### 10.2 Manifest

UTF-8 text, lines ending in LF, no trailing whitespace, in this order:

```
socialmesh-site 1
version <u64>
title <text>
index <path>
[app <path>]
file <attachment index> <path>
…
```

- `version`: for a keyed site, strictly greater than every earlier version
  the author published. Ownerless sites use `1`.
- `title`: 1–200 characters to the end of the line, no control characters.
- `index`: the page shown first; MUST be one of the files.
- An optional `app <path>` line after `index` makes the site an app (§11).
- `file` lines: one per attachment, in ascending byte order of `path`. Every
  attachment is listed exactly once.
- A `path` is 1–255 bytes of segments joined by `/`. Each segment is 1 or
  more characters from `[A-Za-z0-9._~-]` and is not `.` or `..`. Paths are
  case-sensitive and unique.

Anything else (another first line, an unknown keyword, a bad path, a
duplicate) makes the object an invalid site: receivers MUST NOT show it,
and MAY still relay it as an object.

### 10.3 Versions

For a keyed site, viewers show the version with the highest `version`
among the site objects signed by that key; on a tie, the one with the
smallest object ID. Old versions stay ordinary objects and age out of
caches like anything else.

A publisher SHOULD reuse an earlier version's attachment (same manifest
blob and key) for any file whose content is unchanged, so an update only
moves the files that changed.

### 10.4 Viewing

Sites are untrusted. A viewer MUST:

- run no scripts (no `<script>`, no event handler attributes, no
  `javascript:` URLs);
- load nothing from outside the site: no network requests of any kind for
  images, styles, fonts, media, frames, prefetch or DNS prefetch. A site's
  links to elsewhere are followed only when the user clicks one, and SHOULD
  be confirmed first;
- ignore `<meta http-equiv>` (refresh can't redirect) and disable forms;
- create no frames or embedded objects from site HTML (`iframe`, `frame`,
  `object`, `embed`, `portal`, `fencedframe`): browsers preconnect to a
  frame's address, a real DNS lookup, before their policy refuses it;
- serve each file with its manifest media type, never sniffed;
- resolve relative paths within the site only; a missing file is a missing
  file, never a fallback to anything outside the site.

A viewer that serves the site to a browser over loopback MUST put an
unguessable token in every URL and check the `Host` header, so other pages
and programs on the machine can't read the site through it.

Fetching a site's files works like any attachment (§5.4.1). Requesting them
reveals interest to the peers asked, as for any attachment.

### 10.5 Limits

| Item | Limit |
|---|---|
| Files per site | 64 (the attachment limit) |
| File | the attachment limit |
| Whole site | 32 MiB (node policy, SHOULD; phones may hold less) |

## 11. Apps

An app is a site (§10) with code: polls, petitions, sign-ups, surveys. Its
code runs with no network access of any kind, and everything people send
it is an ordinary object, passed around like posts.

### 11.1 App manifest

A site manifest becomes an app with one more line, after `index`:

```
app <path>
```

`<path>` is one of the site's files, ending in `.js`: the app's script.
A viewer that doesn't support apps MUST treat the object as an invalid
site (§10.2), not show it as a static one.

### 11.2 Where app code runs

App code MUST run where it can reach nothing outside the viewer:

- In a browser engine, the app's script runs in a **dedicated worker**
  started by the viewer, never in the page. A page realm exposes APIs a
  content security policy doesn't govern (WebRTC); a worker has none.
- The page runs only the viewer's own host script. Its policy allows that
  one script (by nonce) and nothing else: the site's `<script>` elements,
  event-handler attributes and `javascript:` URLs never run.
- The worker's policy allows scripts from the site only (`importScripts`)
  and no connections, workers or other fetches (`default-src 'none';
  script-src 'self'`). WebAssembly and `eval` stay off.

The app draws by sending HTML to the host, which inserts it after the same
neutralising as site pages (§10.4). The host forwards `click`, `submit`
and `change` events (the nearest `data-action`, and form values) to the
worker.

### 11.3 What an app may do

| Request | What the viewer does |
|---|---|
| `render(html)` | Shows `html` in the app's area. |
| `entries()` | Returns every entry (§11.4) for this app held here. |
| `submit(data, summary)` | Asks the person, showing the app, the summary and the exact `data`, and who it will be published as. Publishes only on yes. |
| `storage` | A small key-value store for this app on this device, never shared. |

There is nothing else: no reading other sites, posts, identities, files or
the network.

### 11.4 Entries

What people send an app is an **entry**: an object (§3) with

- `kind` = `app.entry`;
- `parent` = the ID of the app version they used;
- `body_type` = `application/json`, `body` = the app's `data`, at most
  16 384 bytes of UTF-8 (the app defines the format);
- no attachments.

An app's entries are the entries whose parent is any version of it. Every
entry is public: anyone holding the app can read it.

By default an entry is signed by the person's **per-app anonymous key**,
the thread-scoped key of §1.4 with the app's address digest as
`root_thread_id`. The same person signs every entry to one app with the
same key (so an app can count one answer each), and nothing links that key
to their identity or to other apps. Anonymous entries take the private
route (§5.5) with coarse times, like anonymous posts. A person may choose
to sign with their posting identity instead.

Counts are advisory: anyone can make many keys.
