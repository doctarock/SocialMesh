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
| Bluetooth LE | BLE advertisement w/ service UUID | L2CAP CoC stream / GATT pipe | ~10–100 KB/s | Android, iOS, desktop | **core done**; Android driver written, awaiting phone testing; iOS / Windows next |
| NFC | NDEF tag/tap carrying `mesh1:` | — (hands off to BLE/Wi-Fi) | tiny | Android, iOS (read) | planned |
| QR code | camera scan of `mesh1:` | — | tiny | all with camera | planned |
| Audio (speaker → mic, calls, radio) | — | 16-FSK modem + fountain-coded datagrams, one way | 25–50 bytes/s | all with speaker+mic | **WAV files done**; live speaker/mic next |
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

**Bluetooth LE (core done; platform drivers next).** Wire details are in
SPEC.md §9. Nodes advertise the SocialMesh service UUID with 10 bytes:
version, L2CAP channel number and a random token. There is **no identity**
in the advert. Phones in range find each other and link up with nobody
being told anything: the node treats an advert like a LAN beacon.

- **One link per pair.** When both phones advertise, the one with the
  smaller token connects (the other steps in after 20 s if nothing
  happened). A phone already linked to a device doesn't report it again.
- **Streams.** Links are L2CAP connection-oriented channels where the
  platform has them, otherwise a GATT pipe (write / notify). Either way the
  crate cuts the node's stream into packets that fit the link and joins
  them back, and the node's secure channel runs over it as on any medium.
- **Address rotation.** When the link key rotates (daily, and on network
  change) the radio switches to a new random address and the advert to a
  new token, in the same moment.
- **Listen-only.** `--no-beacon` means the radio never advertises; it scans
  and connects, and nobody nearby can tell it is there until it does.

The radio itself is driven by a **host driver**, because every platform's
Bluetooth API is different and phones expose it only to the app's own
language. The driver gets commands (`Listen`, `Advertise`, `Scan`,
`Connect`, `Send`, `Close`, `RotateAddress`) and reports events (`Advert`,
`Incoming`, `Connected`, `Data`, `Closed`, …). That's everything a Kotlin
(Android), Swift (iOS), WinRT (Windows) or BlueZ (Linux) driver has to
implement; the rest lives in `mesh-transport-ble`. A simulated driver
(`sim::SimAir`: devices on a plane, range, packet size, throughput) runs the
tests: discovery, three-hop relaying between phones out of range of each
other, walking out of range and back, listen-only phones, address rotation,
and a 150 KB file over 64-byte packets.

**Android** (`android/`, `apps/mesh-mobile`): the driver is
`BleDriver.kt` with `Link.kt` and `GattPipe.kt`, radio work only. It accepts
links on an insecure L2CAP channel and on the GATT pipe (for Windows and
iOS peers), and connects with whichever the peer advertises; no pairing,
since the node's encryption covers the link. A foreground service keeps
the node running with the app closed. The node runs in Rust
as `libsocialmesh.so`; driver and node exchange commands and events in a
small byte encoding (`mesh_transport_ble::codec`), with fixed test vectors
on both sides. A test drives two phone nodes through exactly that encoding
into the simulated air. Not yet tried on real phones. Android picks its own
Bluetooth address and rotates it about every 15 minutes; the app can only
restart advertising when the link key rotates. Build steps:
[android/README.md](../android/README.md).

Fine for text posts and small images; large media wants Wi-Fi.

**Wi-Fi Aware / Wi-Fi Direct.** The best proximity medium where available.
Peers find each other and form a direct high-bandwidth link with no access
point. Once the link is up it is plain IP, so the existing TCP transport runs
over it unchanged.

**NFC.** Too short-range and slow for sync, which makes it ideal for
*introductions*. A tap exchanges a `mesh1:` invitation (identity pinned). The
devices then sync over BLE or Wi-Fi. NFC therefore implements `Discovery` only,
and its sightings carry an identity.

**Audio (done, as WAV files).** One-way: `mesh audio send` turns public
posts into a sound file; `mesh audio receive` reads them back from any
recording. Anything that carries voice can carry it: a phone held to a
speaker, a phone call, a walkie-talkie, a PA system, a radio broadcast, a
voice message. Tones sit in the voice band (1–2.5 kHz).

- Payload: an offline bundle, cut into fountain-coded frames (SPEC.md §8).
  Any set of frames slightly larger than the bundle rebuilds it, so a cough,
  a dropout or tuning in late only costs time.
- Speed: 200 bit/s (robust) or 400 bit/s (`--fast`). One short post with
  50% spare frames is about 15 s fast, 30 s robust. Text only.
- Tested against white noise: robust decodes every frame down to −3 dB
  signal-to-noise, fast to 0 dB; plus 48 kHz recordings, gain changes and
  dropouts. Real rooms add echo, which still needs testing on devices.
- Not encrypted, and the transmitter can be located by ear. Only public
  posts are sent; posts still on the private route are refused.

Next: live speaker/microphone, and the same datagram frames over LoRa and
animated QR codes.

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
