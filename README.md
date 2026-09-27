# SocialMesh
A decentralized messaging and social network protocol
# Distributed Social Protocol v0.1

## 1. Purpose

Build a decentralized, device-hosted information and communication protocol.

The system must allow willing devices to exchange content without requiring a central account provider, identity provider, content host, feed provider, discovery service, DNS name, API service, or central moderation authority.

Core principle:

> If two willing devices can exchange bytes, they can participate in the protocol without permission from a third party.

The protocol is not itself a social network. Social feeds, forums, messaging, publishing and other applications are constructed on top of a distributed object network.

The first reference implementation will be written in Rust.

---

# 2. Design Principles

## 2.1 No central authority

The protocol MUST NOT require:

- central accounts
- central identity directory
- central content server
- central feed server
- mandatory bootstrap server
- mandatory DNS
- central moderation
- global username registry
- certificate authority
- phone number
- email address
- government/civil identity
- geographic identity

Two fresh clients on the same LAN must be capable of establishing a functioning network without Internet access.

---

## 2.2 Identity establishes provenance, not permission

Protocol participation does not require a real-world identity.

Supported publication identities:

### Identified

Persistent cryptographic identity.

```text
author: public-key
signature: valid
```

### Pseudonymous

Persistent cryptographic identity with no required real-world association.

### Thread anonymous

A cryptographic identity used only within one conversation/thread.

This permits:

```text
Anonymous A17:
    message

Anonymous B92:
    reply

Anonymous A17:
    another reply
```

without revealing that A17 participates elsewhere.

### Ownerless

No author identity is attached.

Integrity is established by the object's content hash rather than authorship.

---

# 3. Protocol Layers

The protocol should be separated into:

```text
Application
     │
Social / Feed
     │
Discovery / Relationships
     │
Object Graph
     │
Chunk / Storage
     │
Transport Abstraction
     │
Physical Transport
```

Each layer must avoid assumptions about the layer beneath it wherever practical.

---

# 4. Cryptographic Identity

Each persistent identity consists of a locally generated asymmetric keypair.

The private key NEVER leaves the user's device unless explicitly exported by the user.

Conceptually:

```text
private key
    │
    ▼
public key
    │
    ▼
identity ID
```

Recommended initial implementation:

- Ed25519 signatures
- CSPRNG-generated keys
- identity ID derived from public key

Example:

```text
identity:
    version: 1
    public_key: ...
    id: hash(public_key)
```

The protocol contains no mandatory:

```text
real_name
email
phone
country
age
government_id
username
```

Applications may attach optional profile metadata separately.

---

# 5. Objects

Everything distributed through the system is an immutable object.

Examples:

- post
- reply
- reaction
- profile
- recommendation
- discovery record
- moderation assertion
- revocation
- attachment manifest
- media
- file
- arbitrary application object

Every serialized object receives a cryptographic content hash.

Conceptually:

```text
ObjectID = HASH(canonical_serialized_object)
```

The exact canonical serialization and hashing algorithm MUST be specified by the protocol so different implementations generate identical IDs.

For v0.1 use one modern cryptographic hash consistently, with the algorithm/version encoded so migration remains possible.

---

# 6. Basic Post Object

Conceptual structure:

```text
{
    version,
    type,
    created,
    author,
    parent,
    body,
    attachments,
    signature
}
```

`author` may be:

```text
persistent public key
pseudonymous public key
thread public key
null
```

`parent` may reference another object hash.

This allows threads to exist without a server-side comment database.

---

# 7. Replies

Replies are independent objects referencing another object.

```text
Post A
   │
   ├── Reply B
   │      └── Reply D
   │
   └── Reply C
```

Conceptually:

```text
{
    type: "post",
    parent: HASH_OF_A,
    body: "...",
    ...
}
```

Clients reconstruct discussion trees locally.

There is no canonical thread server.

---

# 8. Thread-Scoped Anonymous Identity

When replying anonymously within a thread, a client may derive or generate a thread-specific keypair.

Preferred conceptual design:

```text
thread_private_key =
    KDF(
        local_anonymous_secret,
        root_thread_object_id
    )
```

This allows the client to recover the same anonymous identity when returning to the thread without storing thousands of independent keys.

The resulting public identity MUST NOT expose a computationally feasible relationship to identities generated for other threads.

Thread anonymity is distinct from transport/origin anonymity.

The UI and protocol documentation MUST NOT imply that a thread-specific pseudonym automatically hides network-origin metadata.

---

# 9. Rich Content

The protocol transports objects rather than defining all possible media.

Posts may contain structured text plus references to arbitrary binary objects.

Example:

```text
POST

body:
    markdown/text

attachments:
    - object: HASH_A
      media_type: image/webp

    - object: HASH_B
      media_type: video/mp4

    - object: HASH_C
      media_type: application/pdf
```

v0.1 should support:

- UTF-8 plain text
- Markdown
- arbitrary binary attachments

Applications determine how attachments are rendered.

Arbitrary remote HTML/JavaScript MUST NOT execute as trusted content.

---

# 10. Content Addressing

Binary content is addressed by cryptographic hash.

Example:

```text
photo.webp

        ↓ hash

object: A91F...
```

Any node possessing the correct object may supply it.

The recipient verifies the resulting content hash.

Source and authenticity are separate concepts:

```text
signature
    ↓
WHO authored this?

hash
    ↓
IS this exactly the requested object?

transport
    ↓
WHERE can I obtain the bytes?
```

---

# 11. Chunking

Large binary objects are divided into bounded chunks.

Example:

```text
Video Manifest
│
├── Chunk A
├── Chunk B
├── Chunk C
├── Chunk D
└── Chunk E
```

Each chunk is independently content-addressed.

The manifest contains:

```text
object_size
media_type
chunk_size
ordered_chunk_hashes
```

The receiving client reconstructs the object and verifies the final object hash.

Chunk size should be protocol-defined or selected from a small protocol-defined set.

Start conservatively around tens or hundreds of KiB rather than optimizing prematurely.

---

# 12. Resource Protection

Receiving data MUST NOT imply retaining, indexing, propagating or displaying it.

These are separate operations:

```text
receive
   ↓
validate
   ↓
retain?
   ↓
index?
   ↓
propagate?
   ↓
display?
```

Each client controls each decision.

Nodes enforce local resource budgets.

Example configuration:

```text
max_chunk_size
max_object_size
max_pending_objects
max_cache_size
unknown_peer_bandwidth
trusted_peer_bandwidth
connection_limit
```

A node MUST retain enough capacity for established/trusted peers so unknown traffic cannot trivially exhaust all resources.

---

# 13. Discovery

Discovery is based primarily on peer relationships.

There is no mandatory global identity directory.

A peer may provide signed discovery records describing another identity.

Conceptually:

```text
DISCOVERY

identity: PK_X
profile: HASH
feed_head: HASH

policy:
    discoverable: true
    hops: 3

expires: ...
sequence: ...

signature: PK_X
```

Discovery information can propagate independently from actual content.

---

# 14. Discovery Levels

User-facing discovery should initially expose:

```text
PRIVATE
Directly connected peers only.

FRIENDS
May be discovered one hop away.

EXTENDED
May propagate several hops.

PUBLIC
May propagate without an explicit hop limit.
```

Internally:

```text
private   = 0
friends   = 1
extended  = configurable N
public    = unlimited
```

Two independent permissions exist:

### Publisher permission

May information about me propagate?

### Relay permission

Will my device participate in propagating discovery records?

Both must permit propagation.

---

# 15. Feed Construction

There is no globally stored user feed.

A feed is constructed locally from reachable objects.

```text
Alice ─┐
Bob ───┤
Carol ─┼──► LOCAL INDEX
Dave ──┤         │
Eve ───┘         ▼
               FILTER
                 │
                 ▼
              MY FEED
```

No remote party needs to know exactly what feed the user sees.

Ranking/filtering belongs to the client.

---

# 16. Filtering

The protocol MUST NOT contain a universal definition of acceptable content.

Clients determine locally:

- identities followed
- identities blocked
- topics shown
- topics hidden
- anonymous content policy
- hop limits
- trust thresholds
- attachment download rules
- cache rules
- propagation rules

Future versions may support signed, subscribable filter/reputation lists.

Those lists remain advisory.

The receiving client retains final authority.

---

# 17. Peer Forwarding

Users may deliberately forward objects to other peers.

The protocol should distinguish:

```text
RELAY
Transport happened.

PROPAGATION
Client automatically permitted dissemination.

FORWARD
User intentionally shared something.

ENDORSEMENT
User explicitly recommends something.
```

These MUST NOT be treated as equivalent.

Transporting an opaque chunk does not constitute a protocol-level endorsement.

---

# 18. Anonymous / Ownerless Content

Objects may contain no author.

Example:

```text
{
    type: "post",
    author: null,
    body: "...",
    ...
}
```

The object's integrity remains verifiable through its hash.

Ownerless objects have no cryptographic author capable of later issuing an authoritative edit or withdrawal.

The client should warn users before ownerless publication that copies may remain distributed indefinitely.

---

# 19. Transport Abstraction

The object protocol MUST NOT depend on one network transport.

Define an abstract transport interface conceptually equivalent to:

```text
Transport

discover()
connect()
send()
receive()
disconnect()
capabilities()
```

Initial implementations:

```text
TCP
LAN discovery
File/offline bundle
```

Future possibilities:

```text
QUIC
WebRTC
Bluetooth
Wi-Fi Direct
NFC invitation exchange
LoRa
mesh radio
other transports
```

The object layer should not need modification when a new transport is introduced.

---

# 20. Offline / Store-and-Forward

Offline exchange is a first-class feature.

A user may export protocol objects to a bundle:

```text
network.bundle

discovery records
object manifests
objects
chunks
relationships
revocations
```

Another client imports the bundle.

Imported objects are processed using the same validation pipeline as network-received objects.

Therefore:

```text
Internet transfer
LAN transfer
Bluetooth transfer
USB transfer
SD card transfer
file transfer
```

are conceptually just different means of transporting protocol objects.

---

# 21. Bootstrap

No single bootstrap service may be mandatory.

Possible bootstrap mechanisms:

```text
manual peer address
invitation string
QR invitation
LAN discovery
known peer cache
optional public bootstrap peers
offline bundle
```

The system MUST continue functioning if every project-operated public service disappears.

---

# 22. Invitations

A user should eventually be able to share something equivalent to:

```text
mesh1:...
```

containing sufficient information to introduce an identity and optional current peer hints.

Potential contents:

```text
public identity
discovery policy
optional peer hints
optional capability
introducer signature
```

Peer hints are advisory, not authoritative.

---

# 23. Local Storage

v0.1 should use SQLite for indexes/metadata.

Large binary chunks should preferably be stored as content-addressed files rather than database blobs.

Example:

```text
data/
├── node.db
└── objects/
    ├── 00/
    ├── 01/
    ├── ...
    └── ff/
```

Hash-prefix directories prevent excessively large single directories.

---

# 24. Rust Workspace

Suggested repository:

```text
mesh/
├── Cargo.toml
├── SPEC.md
├── THREAT-MODEL.md
├── README.md
│
├── crates/
│   ├── mesh-crypto/
│   ├── mesh-object/
│   ├── mesh-store/
│   ├── mesh-discovery/
│   ├── mesh-transport/
│   ├── mesh-node/
│   └── mesh-protocol/
│
├── transports/
│   ├── mesh-transport-tcp/
│   ├── mesh-transport-lan/
│   └── mesh-transport-file/
│
├── apps/
│   └── mesh-cli/
│
└── tests/
    ├── two-node/
    └── three-node/
```

---

# 25. Suggested Rust Components

Use mature, maintained libraries rather than implementing cryptographic primitives manually.

Likely categories:

```text
async runtime
serialization
cryptography
hashing
SQLite
CLI parsing
structured logging
error handling
```

Dependencies must remain replaceable implementation choices unless interoperability requires otherwise.

Cryptographic and serialization choices that affect interoperability MUST be specified explicitly in SPEC.md.

---

# 26. CLI

Initial commands should be approximately:

```text
mesh init

mesh identity show

mesh run

mesh peer list

mesh peer add <invitation/address>

mesh publish "Hello world"

mesh publish --anonymous "Hello"

mesh reply <object-id> "Reply"

mesh feed

mesh object show <object-id>

mesh export <file>

mesh import <file>
```

Exact command names may evolve.

---

# 27. Milestone 1 — Two Nodes

Create two fresh nodes.

Neither requires Internet access.

Expected sequence:

```text
A init
B init

A discovers B via LAN

A publishes:
    Hello from A

B receives object

B verifies:
    object hash
    signature

B displays:
    Hello from A
```

Success requires no external service.

---

# 28. Milestone 2 — Three Nodes

Topology:

```text
A ←→ B ←→ C

A and C are NOT directly connected.
```

A publishes.

B receives.

B propagates according to policy.

C receives.

C reconstructs the post.

Then C replies.

The reply travels:

```text
C → B → A
```

A reconstructs the conversation.

---

# 29. Milestone 3 — Anonymous Thread

A publishes an ownerless post.

C replies using a thread-scoped anonymous identity.

A sees:

```text
Anonymous A17:
    reply
```

C replies again.

A must see the same anonymous thread identity.

C participates in another thread.

The anonymous identity MUST differ.

---

# 30. Milestone 4 — Rich Attachment

A publishes:

```text
text
+
image attachment
```

The image is:

```text
hashed
chunked
manifested
transferred
reconstructed
verified
```

C receives the post through B.

C requests the attachment.

Chunks may arrive through B.

C verifies the final hash.

---

# 31. Milestone 5 — Network Independence

Start:

```text
A ←→ B ←→ C
```

Allow discovery/content propagation.

Then permanently stop A.

Expected:

```text
B ←→ C
```

continues functioning.

B and C can:

```text
publish
reply
discover locally
exchange attachments
export/import objects
```

No dependency on A may remain.

---

# 32. Milestone 6 — Offline Exchange

Disconnect A and B completely.

A publishes several objects.

A executes:

```text
mesh export exchange.bundle
```

Transfer file manually.

B:

```text
mesh import exchange.bundle
```

B reconstructs the objects and threads exactly as if they had arrived through TCP.

---

# 33. Threat Model

Create THREAT-MODEL.md covering at minimum:

### Hostile peers

Peers may send:

- malformed objects
- invalid signatures
- oversized objects
- duplicate chunks
- invalid manifests
- connection floods
- excessive discovery records

Clients must fail safely.

### Sybil identities

Generating identities is cheap.

Trust MUST NOT simply equal number of identities.

### Resource exhaustion

Every externally controlled resource must have limits.

### Malicious content

Receiving bytes does not mean rendering or executing them.

Attachments are untrusted.

### Metadata leakage

v0.1 DOES NOT promise strong network-origin anonymity.

Document clearly that:

```text
anonymous authorship
!=
anonymous network origin
```

Future origin-privacy work must receive a separate design and security review.

### Compromised peers

Never assume another peer correctly validated an object.

Every node independently verifies objects.

---

# 34. Explicit Non-Goals for v0.1

Do NOT implement yet:

- blockchain
- cryptocurrency
- proof-of-work economy
- global consensus
- global reputation
- global moderation
- global usernames
- Tor replacement
- custom cryptographic primitives
- elaborate GUI
- mobile applications
- recommendation AI
- distributed search engine
- video streaming optimization
- NAT traversal infrastructure
- perfect network anonymity

These can be evaluated after the basic protocol works.

---

# 35. Protocol Invariants

These should be treated as architectural tests.

## Invariant 1

No central account is required.

## Invariant 2

No civil identity is required.

## Invariant 3

No mandatory server is required.

## Invariant 4

No mandatory DNS resolution is required.

## Invariant 5

Objects remain verifiable regardless of transport source.

## Invariant 6

Transport does not imply endorsement.

## Invariant 7

Receiving does not imply retaining.

## Invariant 8

Retaining does not imply propagating.

## Invariant 9

Propagating does not imply displaying.

## Invariant 10

The client controls its own feed.

## Invariant 11

One implementation disappearing must not destroy the protocol.

## Invariant 12

Two willing devices capable of exchanging bytes must be capable of exchanging protocol objects.

---

# 36. First Implementation Task

Do not attempt to implement the entire specification immediately.

Implement the smallest vertical slice:

```text
identity
    ↓
object
    ↓
serialization
    ↓
hash
    ↓
signature
    ↓
TCP transport
    ↓
receive
    ↓
verification
    ↓
SQLite index
    ↓
CLI display
```

Target:

```text
Terminal A:

$ mesh init
Identity: A91F...

$ mesh run
Listening...


Terminal B:

$ mesh init
Identity: 72BC...

$ mesh peer add 192.168.1.20:7842
Connected A91F...


Terminal A:

$ mesh publish "Hello distributed world"

Object:
8F91...


Terminal B:

NEW OBJECT

A91F...
Hello distributed world

signature: valid
hash: valid
```

Only after this works should automatic LAN discovery and three-node propagation be added.

---

# 37. Development Rule

Whenever an implementation decision introduces a central dependency, ask:

> Can two independent implementations continue operating if this service disappears permanently?

If the answer is no, the dependency must either be removed or explicitly classified as optional.

---

# 38. Definition of v0.1 Success

v0.1 is successful when three ordinary computers can:

- create identities locally
- discover/connect without central infrastructure
- exchange signed posts
- exchange ownerless posts
- construct replies
- maintain thread-scoped anonymous identities
- propagate objects across an intermediate peer
- exchange arbitrary binary attachments
- enforce local resource limits
- build feeds locally
- export/import objects offline
- continue functioning after the original publishing node disappears

with:

```text
NO account
NO API
NO website
NO DNS requirement
NO central database
NO identity provider
NO mandatory bootstrap service
NO civil identity requirement
```

At that point we have demonstrated the fundamental architecture.

Everything after that is scaling, privacy hardening, transport expansion and application development.
