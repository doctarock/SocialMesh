# Origin Privacy

**Status:** design implemented in v0.1. Needs independent review before anyone
relies on it (README §33 requires a separate security review for this area).

## The problem

An anonymous or ownerless post hides its *author key*. But in a gossip network
the first node to announce an object is very likely its author. Whoever is
connected to that node, or watching its traffic, learns which device
published it. That defeats the purpose of anonymous posting.

The first implementation leaked origin in four ways:

| Leak | Why it identified the author |
|---|---|
| First announcer | The author's node announced its own post before anyone else could |
| `--no-relay` | A non-relaying node only offered its own objects, so everything it offered was authored there |
| Timing | Announced within 500 ms of a millisecond-precision `created_ms` |
| Disk | Stored as `origin = local`, so a seized device proved authorship |

## Threat levels

Be clear about who this defends against.

| Adversary | Example | Defended in v0.1? |
|---|---|---|
| **A1** One curious peer | Someone you sync with | **Yes**: they can't tell your posts from ones you relay |
| **A2** A few colluding peers | Several monitoring nodes joined to the network | **Partly**: they must surround the author to be confident; risk falls as the network grows |
| **A3** Your ISP / Wi-Fi operator | Watches your link | **Partly**: traffic is encrypted and padded, so stems look like any other traffic. Still sees *that* you sent something, roughly how much, when, and to whom |
| **A4** Global passive observer | Watches every link | **No.** Needs an onion/mixnet transport (see below) |
| **A5** Device seizure | Phone or laptop taken | **Mostly**: the disk doesn't distinguish your anonymous posts from relayed ones (see residual risks) |

## Design: two-phase propagation (after Dandelion++)

Based on *Dandelion++: Lightweight Cryptocurrency Networking with Formal
Anonymity Guarantees* (Fanti et al., 2018), which addresses this exact problem
for Bitcoin transactions. It needs no new cryptography.

```text
             STEM phase (private, one hop at a time)          FLUFF phase (normal gossip)

author ──STEM──► relay ──STEM──► relay ──STEM──► relay ══HAVE══► everyone
                                            (coin flip: p = 0.1 per hop)
```

**Stem phase.** The object travels as a pushed `STEM` message to a single
successor at each hop. While in the stem it is **embargoed**: no node offers
it in HAVE, and no node serves it in response to a WANT, even when the
requester guesses the ID.

**Fluff phase.** At each relay hop, with probability `p` (default 0.1, about
10 hops on average), the relay ends the stem and announces the object
normally. From then on the object spreads like any other.

### Signed posts take the same route

The route isn't only for anonymous posts. A signed post says who wrote it,
but if it is announced from the author's phone, anyone nearby learns which
phone is X's: the post appears there before anywhere else. So the reference
client sends signed posts the private way too, by default: same author delay,
same coarse timestamp, same cache storage for files, public from a relay.
`--direct` announces straight from the device, for accounts that don't need
this (an organisation's official feed) or files over the 16 MiB route limit.

### Rules that make author and relay indistinguishable

1. **Identical storage.** Authored-anonymous and relayed-stem objects are both
   stored as `Origin::Stem`. The only author-specific record is a
   `stem_outbox` row, deleted as soon as the object is pushed (normally under
   a second while a relay peer is connected).
2. **The author never fluffs at hop 0.** Otherwise the first fluffer would be
   the author with probability `p`.
3. **No fallback to announcing.** With no stem-capable peer the author waits.
   It does not broadcast instead.
4. **Stable routes per epoch.** Each node keeps the same stem successor for
   10 minutes. If successors changed per message, a peer could compare routes
   across messages and work out who sent what.
5. **Quiet un-embargo.** When a node sees an embargoed object offered by
   someone else, it lifts the embargo and requeues the object so it is
   announced on the normal schedule. An instant re-announce would mark the
   node as having been on the stem; a regression test covers this.
6. **Coarse timestamps.** Anonymous objects round `created_ms` down to the
   minute.
7. **Relay consent.** Only nodes advertising `FLAG_ACCEPTS_STEM` (relaying
   enabled) are sent stems, and non-relay nodes reject them.

### Attachments

An anonymous post's file is the weak point: if only the author holds it, the
first node to serve it is the author. So:

1. **Seal at creation.** The file is encrypted under a random key carried only
   in the post, and split into uniform 64 KiB pieces. Pieces look the same
   whether they are a manifest or a chunk, whatever file they belong to.
2. **Carry it along the stem.** Each hop pushes all pieces (STASH) to the
   next hop *before* the post, on one ordered queue. By the time anyone makes
   the post public, every node on the route holds the whole file and serves
   it like any other cached data.
3. **Store it like cache.** The author's pieces are recorded exactly as a
   relay's: source "cache", no peer, no time.
4. **Don't forward what you can't carry.** A relay missing pieces ends the
   stem (fluffs) instead of passing on a post whose file only upstream nodes
   hold.

5. **Pace and cover.** Pieces leave at a jittered pace, never in one burst,
   and the fallback timer starts only after the hand-over (scaled by file
   size), so slow pacing can't make the author publish its own post. Every
   node also sends decoy runs: random pieces, same pacing, random lengths and
   times, ended by a PAD where a real run ends with a STEM. To someone
   watching the author's link, the real run is one more run.

Limit: 16 MiB of attachments per anonymous post, because every hop carries
the whole file.

### Failure handling

Each stem holder sets a random embargo timer (20–50 s). If it hasn't seen the
object go public by then, it fluffs the object itself. A hostile relay can
therefore delay an anonymous post but cannot black-hole it. Loops end at the
first repeated node, which fluffs.

## Residual risks (honest list)

- **Small networks give little cover.** With three nodes, "one of these three"
  is all the anonymity there is. The protection grows with the number of
  honest relays.
- **First-hop adversary.** If your *only* relay peer is hostile, it receives
  your stem first. It can't prove you wrote the post rather than relayed it,
  but on a sparse network it will suspect you. Connect to several peers.
- **Send timing on the wire.** Transport encryption hides which messages are
  stems, but an observer of your link still sees encrypted bytes leave
  shortly after you were quiet. Cover traffic would blur this.
- **Timing at hop 0.** Relays forward a stem immediately on receipt, but the
  author's first push happens on the next outbox poll (≤ 500 ms) after
  `mesh publish` runs. There is no inbound STEM just before it, which a link
  observer could notice. Fix candidates: cover traffic, or random hold at every
  hop (at a latency cost).
- **Quiet un-embargo timing** is about one round-trip faster than a genuine
  HAVE → WANT → OBJECT fetch. Small, but measurable by a patient peer.
- **Non-relay authors have no cover.** A `--no-relay` node only holds its own
  stem objects, so if it is ever forced to fluff, origin is obvious. Such
  nodes therefore wait rather than fluff, and the CLI should warn about this.
- **Device seizure while queued.** If the device is taken before the outbox
  drains (for example, no peers were available), the outbox row is evidence.
- **Cover is a dial, not a guarantee.** At the default 16 MiB/hour, decoy
  runs happen every few minutes on average. An observer who sees a run
  shortly after you were seen at a protest still learns "one of the runs".
  More cover blurs more; zero cover gives no protection for file sends. Cover
  also costs data (about 380 MiB a day at the default), which matters on
  phones.
- **Short-run statistics.** Run lengths are drawn log-uniformly; a patient
  observer comparing many of your runs against that distribution could spot
  an excess at one particular length. Mixing in real cached pieces and
  adapting rates to activity would help.
- **Writing style, content and metadata** inside a post or its files can
  identify an author regardless of routing. Strip image metadata.

## What comes next

1. ~~Encrypted transport~~: done (SPEC.md §5.0). Noise `NN` with HELLO
   signatures bound to the handshake hash.
2. **Onion transport** (Tor via a mature Rust client such as Arti) as a
   pluggable `Transport`. This is the only realistic defence against A4, and it
   slots in without protocol changes.
3. ~~Cover traffic~~ for attachment runs: done (SPEC.md §5.6). Still open:
   cover for plain anonymous posts (a single STEM) and adaptive rates.
4. **Encrypted storage** with a passphrase, to shrink A5 further.
5. **External review** of all of the above.
