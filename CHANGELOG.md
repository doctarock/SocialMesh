# Changelog

## Unreleased

- **Sites** (SPEC.md §10): folders of web pages published as objects and
  passed around like posts. Each site has its own key, separate from your
  identity, which is its address and lets you publish new versions; only
  changed files travel. Ownerless sites are anonymous and can't be updated.
  `mesh site new | publish | list | fetch | view`.
- Safe site viewer: no scripts, nothing loaded from outside the site, no
  redirects or connection hints, forms disabled. It serves your browser on
  127.0.0.1 behind a random token, with a Host check. Checked in a real
  browser: a test page's tracking image, preconnect and redirect made no
  network request.
- Offline bundles version 2 carry attachment and site files, not just
  posts (`mesh export --no-files` for objects only).
- The feed shows posts only; sites are listed with `mesh site list`.

## 0.1.0 (2026-11-16)

First public release: a working proof of concept. **Experimental and not
independently security-reviewed**; see [THREAT-MODEL.md](THREAT-MODEL.md)
before relying on it for anything that matters.

### Protocol and node

- Local identities, with no accounts, servers, DNS or registration. Signed,
  ownerless (unsigned) and thread-scoped anonymous posts and replies.
- Content-addressed, signed objects (SPEC.md). Feeds are built locally.
- Every link encrypted (Noise, forward secrecy). Link keys are separate from
  posting identities and rotate.
- Anonymous posts take a stem/fluff route, so they go public from a relay a
  few hops away and not from the author. Signed posts can do the same with
  `--private-route`. Cover traffic and decoy messages.
- Attachments are sealed, cut into uniform 64 KiB pieces, fetched from several
  peers at once and verified per piece. Relays cache them without being able
  to read them.
- The network keeps working after the original publisher disappears.
- Local resource limits: cache budgets, object and attachment size caps.

### Media

- TCP over LAN or the internet, with LAN discovery and no bootstrap server.
- Offline bundles (export/import) for USB sticks and sneakernet.
- Bluetooth LE: phones in range find each other and link up by themselves.
  Adverts carry no identity, and the radio address changes with the link key.
  Links use L2CAP channels, or a GATT pipe for platforms without them.
- Sound: public posts as tones (16-FSK, 200 or 400 bit/s), carried by
  fountain-coded frames with per-frame Reed–Solomon. Works speaker to
  microphone, over calls, radios and recordings.
- One node uses all its media at once and links to the same peer over each.

### Apps

- `mesh`: the command-line node (Windows, macOS, Linux).
- Android test app (Android 10+): Bluetooth and Wi-Fi sync, a background
  service, and posting by sound (Play / Listen). Tested on two phones.

### Known limits

- Keys are stored unencrypted on disk.
- No independent review yet of the secure channel, the handshake or the
  object format.
- Link observers can still see who connects to whom and when. Not safe against
  state-level surveillance.
- Only an Android driver for Bluetooth so far: no iOS, Windows or Linux
  driver yet. The Android app is a test app, not a finished product.
- The sound modem is tuned on one pair of phones in one room.
