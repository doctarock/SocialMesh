# Threat Model v0.1

Read this before relying on SocialMesh for anything sensitive. v0.1 proves the
architecture. It is **not** yet a tool for people who face a capable
adversary, such as state surveillance.

## What v0.1 protects

| Property | How |
|---|---|
| **Integrity:** content can't be altered in transit or storage | Content-addressed IDs, re-verified on every receive *and* every disk read |
| **Authenticity:** a signed post really came from that key | Ed25519 strict verification with domain-separated signing input |
| **Confidentiality in transit:** link observers can't read traffic | Noise `NN` handshake (X25519, ChaCha20-Poly1305, BLAKE2s) with fresh keys per connection; records padded to 256-byte size classes |
| **Forward secrecy:** a recording can't be decrypted later by stealing keys | Connection keys are ephemeral and never stored; identity keys only sign |
| **Peer identity:** you're talking to the key you think | Each side signs the connection's unique handshake hash; identity pinning for invitations and known peers |
| **Device ≠ author:** connecting to a device doesn't reveal who it posts as | Links prove a separate link key, never the posting key. User nodes rotate it daily and on network change; relays have no posting identity at all |
| **Dialler identity hiding** | The node being dialled proves itself first. A dialler with a pinned identity hangs up on an impostor without revealing who it is, and `peer add` never reveals it |
| **No chokepoint:** nobody can switch the network off centrally | No accounts, servers, DNS or bootstrap service; LAN and offline bundles work with no Internet |
| **Authorship unlinkability across threads** | Thread keys derived from a local secret; not linkable without it |

## What v0.1 does NOT protect

### Network-origin anonymity: **partial: peers yes, link observers see timing only**

```
anonymous authorship  !=  anonymous network origin
```

Anonymous posts use two-phase stem/fluff routing (docs/ORIGIN-PRIVACY.md).
A post travels privately through a random chain of relays before any node
makes it public, and your node handles your own anonymous posts exactly as it
handles ones it relays. Peers therefore can't tell which posts you wrote.

Someone watching your network link (your ISP, the Wi-Fi operator) can no
longer read which messages are stems or which posts they carry. They can still
see *that* your device sent encrypted data, how much (to the nearest 256
bytes), when, and to which IP addresses. A burst sent while you were otherwise
quiet is a hint. A state-level observer watching many links at once can
correlate timing across the network. Strong anonymity against that needs an
onion transport such as Tor. Small networks also give little cover. Tell users
this plainly in any UI.

### What transport encryption does not hide

Encryption protects the *link*. It does not make anything private from the
mesh: once fluffed, every post is public to every node. Specifically:

- **Who you connect to.** IP addresses, ports, connection times and traffic
  volume are visible to anyone on the path.
- **That you run SocialMesh.** The traffic is not disguised. The first packet
  is a 34-byte handshake message, and record sizes cluster on a 256-byte grid.
  A censor using deep packet inspection could identify and block it.
  Defeating that needs pluggable obfuscation transports (as Tor bridges use)
  or an onion transport.
- **A listener's link key.** The responder proves its link key to anyone who
  connects, so a reachable node's link key is effectively public. That's a
  device pseudonym, not the owner's posting identity. A stable link key (for
  invitations) lets peers recognise the same device over time; a rotating one
  limits that to a day or one network.
- **Who first offers a `--direct` post.** Signed posts take the private route
  by default and become public from a relay, not the author's device. With
  `--direct` the author's device announces the post itself. A peer connected
  to it (anyone nearby, for a phone) then sees a new post signed by X appear
  from that device first, and can conclude "this device is X".
- **A signed identity's own history.** The private route hides which device
  posted, not who: everything signed by X is linkable to X by design. Someone
  who already knows a device belongs to X learns nothing new; someone
  watching a device can't use X's posts to confirm it is X's.
- **An unpinned dialler's identity.** It is revealed to whichever node answers,
  once that node has proved *some* identity. Pinned dials and LAN sightings
  with an identity hint are protected; plain addresses are not.
- **Post-quantum.** X25519 is not quantum-resistant. A recording made today
  could in principle be decrypted by a future quantum computer. A hybrid
  key exchange (e.g. ML-KEM + X25519) is a candidate for a future version.

### Several media at once

Peers connected over several media spread each transfer across them
(SPEC.md §5.3.1). An observer of one medium sees only part of a transfer:
fewer pieces, a shorter run. An observer of several media in the same place
(a radio sniffer that covers both Bluetooth and Wi-Fi) can line the parts up
by timing. Each radio a device uses is another way to notice it is there.

### Presence on a local network

LAN beacons do not contain an identity. They do reveal that *a* SocialMesh node
is on the network, on a fixed UDP port. In settings where running the software
is itself risky, use `mesh run --no-beacon` (listen only) or `--no-lan`.

### Device seizure

`identity.key` and `anonymous.key` are stored **unencrypted**. Whoever gets
the data directory can sign as you and link every one of your thread-anonymous
replies. Planned: passphrase encryption at rest.

Anonymous posts themselves are stored identically to anonymous posts relayed
for others, so the object store alone doesn't show which ones you wrote. The
exception is the brief `stem_outbox` record kept until a post enters routing.

### Inventory disclosure

On connect, a node advertises every object it is willing to share. A peer
therefore learns what you hold. With relaying on, that includes content you
merely passed along.

### Attachments

- **Sealed at creation.** Every attachment is encrypted with a random key that
  exists only in its post. Relays and caches hold uniform 64 KiB pieces they
  can't read or group into files. Anyone who has the post can open the file,
  so this hides content from *caches and links*, not from readers.
- **Anonymous attachments travel the stem.** The pieces go ahead of the post
  along the anonymity route, so every relay on it holds the whole file by
  the time the post goes public. The author's copy is stored exactly like a
  relay's.
- **Sends are paced and covered.** Pieces leave at a jittered pace, and every
  node sends decoy runs of identical-looking pieces at random times
  (16 MiB/hour by default). A link observer sees runs of pieces either way.
  The protection is only as strong as the cover: turn it off
  (`--cover-mib-per-hour 0`) and a real run after a quiet period stands out
  again. Anonymous attachments are capped at 16 MiB, so real runs stay inside
  the range decoys cover.
- **Asking reveals interest.** Fetching an attachment tells the peers you ask
  that you wanted those pieces. Anyone who also has the post can link the two.
- **Metadata is stripped.** JPEG, PNG and WebP keep only the image data:
  EXIF (GPS, time, device, thumbnail), XMP, IPTC, comments, text chunks,
  embedded extra images and anything appended after the image are removed.
  Other types that can carry metadata (PDF, video, audio, office documents,
  HEIC, GIF) are refused unless the user passes `--keep-metadata`. Stripping
  can't remove what is *in* the picture: faces, street signs, reflections.
- **Storage is budgeted.** Cached data from others is capped (2 GiB default).
  Decoys and orphaned pieces are evicted first, then the oldest cached files.

## Hostile peers

| Attack | Mitigation |
|---|---|
| Malformed / truncated / oversized objects | Strict canonical decoder; size checks before allocation; rejected objects count as strikes, and the peer is dropped after 16 |
| Forged signatures, stripped signatures | Signature required iff author present; strict verify |
| Unsolicited object flooding | Objects are accepted only if requested on that connection |
| Frame-length bombs | Length checked against the limit before allocating |
| HAVE floods | ≤1 024 IDs per message; ≤4 096 outstanding WANTs per connection, expiring after 60 s |
| Connection floods | Global connection cap (64) |
| Discovery floods | Repeat sightings rate-limited; discovered-address table capped at 1 024 |
| Impersonating an invited peer | Identity pinning: the handshake must prove the invited key |
| Compromised relay | Every node verifies independently; a relay can drop or delay content but cannot alter it |
| Junk or corrupted chunks | Each blob is named by hashing what arrived; anything not being fetched is discarded, counts as a strike, and stops that peer being asked for 60 s while its requests go elsewhere |
| Inconsistent manifest from the author | Whole file is re-hashed after reassembly; the attachment is marked failed |
| Blob request floods | Requests never block the server: if its outbound queue is full it answers BLOB_MISSING |
| STASH floods | Relays only, fixed 64 KiB size, 1 024 per peer per minute; excess is a strike; cache budget evicts unreferenced pieces first |

## Sybil identities

Identities are free to create. Nothing in v0.1 counts identities as a measure
of trust, and nothing should. Future reputation or filter lists must be
subscribable, advisory, and local (README §16).

## Malicious content

Bodies are displayed as plain text. Attachments are untrusted bytes. No remote
HTML or script is ever executed.

## Known gaps to close before public use

1. Passphrase-protected keys at rest.
2. Clock sanity policy for `created_ms` (currently the author's unverified claim).
3. Cover for receiving: a device fetching a file right after it's posted can hint at interest.
4. Independent security review of the secure channel, handshake and object format.
5. Traffic obfuscation or an onion transport where running SocialMesh is itself risky.
6. Metadata stripping for PDF and video; adaptive cover rates for phones on metered data.
