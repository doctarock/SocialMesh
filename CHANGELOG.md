# Changelog

## Unreleased

- **Apps** (SPEC.md §11): sites with code (polls, petitions, sign-ups).
  App code runs in a web worker with no network at all: the page runs
  only SocialMesh's host script, so WebRTC (which a content security
  policy can't block) is out of reach. People's answers are entries,
  public replies to the app, signed by default with a key unique to them
  and the app, sent by the private route. Every send is confirmed first.
  `mesh site publish --app <script>`, `mesh site entries`; the desktop
  viewer asks in the terminal. Example: `examples/poll`.
- Android: apps open in the phone's viewer, each site on its own origin,
  and the phone asks before an app sends anything (anonymously, as you,
  or not at all).
- Android: Bluetooth is no longer required. The app starts with whatever is
  there and picks Bluetooth up when it's turned on (previously, turning
  Bluetooth off and on stopped it accepting links until a restart).
- Android: **sound modem**. With no Bluetooth and no Wi-Fi, the phone listens
  and takes turns playing its newest public posts every minute or so. It
  listens before talking, including for tones too faint to decode, and
  warns when the media volume is too low to be heard.
- Android: **Hosts**. Add a node by address or `mesh1:` invitation; it's
  pinned to the node that answered and kept in sync over any network. Nodes
  announcing themselves on the local Wi-Fi are listed and can be saved.
- **Security fix:** a site's `<iframe>` made the browser look up the
  frame's address in DNS before its policy refused the frame, telling a
  site author when someone opened the page. Frames and embedded objects
  are now neutralised. Checked against 38 ways HTML can reach out: none
  now touches the network.

## 0.2.0 (2026-11-17)

Sites: web pages passed around like posts, published and viewed on computers
and phones. Still an **experimental proof of concept, not independently
security-reviewed**.

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
- Sound (`mesh audio send`, the app's Play) sends posts only, never a newer
  site, whose files sound is far too slow to carry.
- Android: a **Sites** screen and viewer. Every request is answered by the
  app from the site's files, with scripts, network loads, Safe Browsing
  lookups and WebView metrics off, and outside links opened only after a
  prompt. Tested on a phone: a site and its update arrived over Wi-Fi from a
  computer, and a tracking image was blocked before any request was made.
- Android look and feel: breadcrumbs on every screen (`SocialMesh › Sites ›
  site › page`, each a link back up) and a back arrow; cards and section
  headings that follow light and dark mode; a status line saying who you're
  linked with and how; posts with relative times; the activity log tucked
  away; a confirmation before Stop. The site viewer gains Reload, About this
  site (version, files, who can publish) and Copy address; long-press a site
  to copy its address; publishing previews the files.
- Android: publish a folder or `.zip` as a site (new, update, or anonymous),
  with photo metadata stripped. Tested on a phone, including a version 2
  update with a real photo whose GPS data was removed.

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
