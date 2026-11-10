# Transports & Discovery Roadmap

Goal: a SocialMesh node can find and exchange objects with other nodes over
**any** medium it has available, online or physically nearby.

There are two plug-in points. Nothing above them changes when a medium is added.

```text
Discovery  (mesh-discovery::Discovery)   "a node may be reachable at X"     → Sighting
Transport  (mesh-transport::Transport)   "give me a byte stream to X"       → Stream
```

A medium can implement one or both. Every connection, however it was found,
goes through the same authenticated handshake and the same object verification.
So a new medium adds **reach** without adding **trust**.

## Status

| Medium | Discovery | Transport | Bandwidth | Platform | Status |
|---|---|---|---|---|---|
| Internet / URL | `mesh1:` invitation | TCP | high | all | **done** |
| LAN / Wi-Fi (same network) | UDP broadcast beacon | TCP | high | all | **done** |
| Offline file (USB, SD, email) | — | bundle import/export | any | all | **done** |
| Wi-Fi without a router | Wi-Fi Aware / Direct | TCP over P2P link | high | Android (Aware/Direct), Windows (Direct) | planned |
| Bluetooth LE | BLE advertisement w/ service UUID | GATT / L2CAP CoC stream | ~10–100 KB/s | Android, iOS, desktop central-only | planned |
| NFC | NDEF tag/tap carrying `mesh1:` | — (hands off to BLE/Wi-Fi) | tiny | Android, iOS (read) | planned |
| QR code | camera scan of `mesh1:` | — | tiny | all with camera | planned |
| Audio (speaker → mic) | chirp carrying `mesh1:` / beacon | FSK modem stream | ~10–100 bytes/s | all with speaker+mic | planned |
| Mesh radio (LoRa etc.) | — | packet stream | very low | add-on hardware | later |

## Using every medium at once

Two nodes that can reach each other over several media use **all of them at
the same time**, one link per medium. A phone next to a laptop might hold a
Bluetooth link and a Wi-Fi link to it, while both also reach a relay over the
internet.

- Each medium is a `Transport` registered on the node
  (`Node::add_transport`). Addresses carry the medium as a prefix
  (`ble:…`, `wifi:…`); bare `host:port` means TCP.
- Every link is its own encrypted session. The node treats the peer as one
  peer with several links.
- Traffic is spread over the links:
  - attachment pieces are requested from whichever link is least busy, so a
    fast link carries more and a slow one still helps;
  - anonymous files on the stem are shared out piece by piece, each link
    paced on its own, and the relay reassembles them (it waits if the post
    overtakes pieces still coming over a slower link);
  - HAVE, STEM and cover-run endings go out on a random link.
- If a link drops, pieces it hasn't taken yet go over the others, and pending
  attachment requests are re-sent elsewhere. If both ends dial each
  other, they agree on one link per medium without talking about it.

**What it does for privacy.** Someone watching a single medium (a Bluetooth
sniffer, a Wi-Fi monitor, an ISP) sees only part of each transfer. They can't
count a whole file's pieces, and the run they do see is shorter than the real
one. It does **not** defeat an observer watching several media at the same
spot: they can line up the timing. The device also shows up on each radio it
uses, so more media means more ways to be detected. Turning a medium off is
always an option.

**What it does for capability.** Throughput adds up across links, and a
transfer survives losing any one of them.

## Encryption is not a transport's job

A transport only has to produce a reliable byte stream. The node wraps every
stream in the Noise secure channel (SPEC.md §5.0) before sending anything else.
New transports (BLE, Wi-Fi Direct, audio, WebSocket) are encrypted
automatically and can't opt out. Offline bundles are not encrypted: they only
contain objects that are public anyway. Anyone holding a bundle can read it,
so treat the file like a printed copy.

On very slow links (audio) the handshake costs about 100 bytes plus a
256-byte padded record per frame, which is noticeable at tens of bytes per
second. A future low-bandwidth mode may shrink the padding there.

## Design notes per medium

**Internet / URL.** A node with a public address shares
`mesh1:<id>@host:port`. It also works inside a web link
(`https://anything/#mesh1:…`), so a link can be posted anywhere without that
site having to understand it. Next: a WebSocket transport, so nodes behind
restrictive firewalls, and eventually browser nodes, can be reached through
any ordinary web host. Any such host is optional, never mandatory.

**Bluetooth LE.** Advertise a fixed SocialMesh service UUID with **no
identity** (same rule as LAN beacons), plus a rotating random token. Discovery
reports `ble:<device-address>`; the BLE transport opens an L2CAP
connection-oriented channel (or a GATT fallback) and exposes it as a `Stream`.
Desktop Bluetooth libraries can usually *scan* but not *advertise*, so phones
are the main BLE peers. Fine for text posts; too slow for large media.

**Wi-Fi Aware / Wi-Fi Direct.** The best proximity medium where available.
Peers find each other and form a direct high-bandwidth link with no access
point. Once the link is up it is plain IP, so the existing TCP transport runs
over it unchanged.

**NFC.** Too short-range and slow for sync, which makes it ideal for
*introductions*. A tap exchanges a `mesh1:` invitation (identity pinned). The
devices then sync over BLE or Wi-Fi. NFC therefore implements `Discovery` only,
and its sightings carry an identity.

**Audio.** A fallback for when no radio is usable: an FSK/chirp modem over
speaker and microphone, built from mature DSP code rather than invented
crypto. Realistic throughput is tens of bytes per second, so its main use is
carrying a `mesh1:` invitation (~80 bytes) between two phones in a room.
Bulk transfer at that rate is possible only for short text, using the bundle
format as the payload. Error detection relies on the existing content hashes.

## Privacy rule for every medium

Beacons and adverts MUST NOT carry a stable identity. Identity is revealed
only inside the handshake, to a peer you are actually connecting to, or in an
invitation you deliberately hand someone. Even then it is a **link key**,
never the user's posting identity, and user devices rotate it daily and
whenever they change network. A radio transport should rotate its own
hardware address (Bluetooth address, Wi-Fi MAC) at the same moment, or the
old address links the two keys. Every transmitting medium MUST have
a listen-only mode (`--no-beacon`).

## Adding a medium

1. Implement `Discovery` and/or `Transport`.
2. Register it on the node: `Node::add_discovery` and/or
   `Node::add_transport(transport, listen)`. Its addresses become
   `name:addr`, where `name` is `Transport::name`.
3. Add an integration test. `crates/mesh-node/tests/network.rs` has the
   in-memory `FakeMedium` discovery and the `AltMedium` second transport.
