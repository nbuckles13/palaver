# Palaver — design

Status: draft 3, 2026-11-15. The MoQ-based design, confirmed. It merges the two earlier drafts
kept in `archive/` (the original slot-based design and the MoQ-native alternative).

Terms: a **node** is one running instance (app or headless). A node contains a **client**
(capture, encode, decode, render) and a **khef** (a small MoQ relay of our own). Every khef is an
**edge khef** for its own client; the conductor also uses some as **transit khefs** ("hubs").
One node at a time holds the **conductor** role. An **iroh-relay** is the separate, optional
connection helper from the iroh project (STUN/TURN-like, not MoQ); "relay" alone is not used
for either. MoQT = Media over QUIC Transport; facts about it
are from draft-ietf-moq-transport-23 unless noted (see [archive/moq-evaluation.md](archive/moq-evaluation.md)).

Scope: **25–50 participants**, single conductor.

## 1. Principles

1. **Conductor decides, khefs execute.** The conductor is never in a hot path: not media, not
   subscription fulfilment, not audio selection, not crash reaction. Khefs keep working on their
   last applied plan if it vanishes.
2. **One protocol.** Clients talk MoQT to a khef; khefs talk MoQT to each other; control state
   rides MoQ tracks. MoQT treats a coordinated set of relays as one relay and leaves its insides
   unspecified, so routing, selection and the conductor are ours.
3. **A client has one khef for life.** Multi-path, migration and failover happen between khefs.
   Clients never migrate.
4. **No single-khef special case.** The general multi-khef algorithm always runs.
5. **Connected, not fully meshed.** Nothing assumes a direct link between two particular nodes,
   including to the conductor.
6. **Khefs are media-blind.** They act on names, priorities, properties and group boundaries,
   never on codec or media kind. A track switches source or path only at a group start (video: a
   keyframe; audio: every object).
7. **Pure decision functions.** Khef behaviour and plan computation are sans-IO.
8. **The engine is an SDK.** GUI app, bots, recorders, transcribers and AI participants are all
   consumers of the same API.
9. **MoQ conformance is an edge property.** The network client↔khef edge follows the pinned
   draft for our profile. Khef↔khef links are ours and versioned.

## 2. Node anatomy

```
┌──────────────────────────── node (one process) ────────────────────────────┐
│  app: GUI shell │ headless bot │ recorder │ custom integration             │
│  ───────────── SDK (events out, commands in, raw/encoded media in/out) ─── │
│  client: capture → audio processing → encode ─┐   render ← decode ← jitter │
│                                               │ MoQT, in-memory (no QUIC)  │
│  khef: subscriptions · routing table · top-N · priorities · dedupe        │
│        │ MoQT over QUIC                                                    │
│        └──────────────► other nodes' khefs                                │
│  control: reports out, plan in; conductor role if elected                  │
│  net: QUIC endpoint, hole punching, rendezvous, admission (PAKE)           │
│  ops: metrics ring, structured event log, diagnostics bundle               │
└────────────────────────────────────────────────────────────────────────────┘
```

**The local hop is not a network hop.** Client and khef exchange MoQ-formatted control messages
and reference-counted object buffers over an in-memory channel: no sockets, no QUIC, no TLS,
nothing sent or encrypted twice.

Crates (Rust workspace, permissive licence; code copied from dark_tower where useful, nothing
shared):

| Crate | Contents | IO? |
|---|---|---|
| `moq-wire` | MoQT message and object encoding for the pinned draft, our own codec covering only our profile, isolated so a draft change touches only this crate; holds the parameter table, including our extensions | none (other MoQ codecs as dev-only test oracles) |
| `proto` | Our control payloads: report, plan, roster, commands; naming scheme; test vectors | none |
| `khef-core` | Khef state machine: sessions, subscriptions, routing, top-N, priorities, dedupe, congestion marking | none |
| `topology` | (roster, reports, link graph) → plan | none |
| `control` | Membership, conductor election and hand-over | sans-IO core + driver |
| `net` | QUIC endpoint, in-memory transport, rendezvous, admission handshake | yes |
| `media` | Capture/encode/decode/render/audio-processing/jitter traits, platform impls, synthetic sources | yes |
| `sdk` | Public API; later UniFFI bindings | yes |
| `sim` | Deterministic multi-node simulator: our own discrete-event loop over the sans-IO cores (spike 6) | none |
| `guards`, `e2e` | Guard tests; end-to-end scenarios | tests |
| `apps/bot`, `apps/harness` | Headless client; egui dev UI | yes |

## 3. Discovery and admission

Inputs: **meeting id + password** (or a link encoding both). Names are user-provided; SSO/org
directory is a later layer.

```
root    = Argon2id(password, salt = meeting_id)
K_topic = HKDF(root, "topic" ‖ time_epoch)   → rendezvous key; encrypts announcements
K_auth  = HKDF(root, "auth")                 → never published; input to a PAKE
```

- **Rendezvous** is pluggable and merged: invite ticket (inviter's key and addresses), mDNS/LAN,
  mainline DHT under `K_topic`, optionally Nostr relays or a public MoQ relay network.
- **Reachability**: public STUN-like address discovery, port mapping, IPv6, hole punching with
  members as introducers. An **iroh-relay** (enabled by default, URL list configurable) covers
  what members cannot: first contact when no member is publicly reachable, and a last-resort
  path for pairs that cannot punch. Its path is QUIC tunnelled over TCP, so it is poor for
  media; a headless Palaver node on a reachable machine is the server-free alternative.
- **Admission**: every new connection runs a balanced PAKE (CPace or SPAKE2) on `K_auth`, bound
  to both transport keys, before any MoQ message. Node identity is a per-meeting Ed25519 key.
- **Host**: the meeting creator. Host is a roster attribute independent of the conductor role and
  can remote-mute, kick and lock. On host leave it passes to the longest-present member (a
  co-host role later).

## 4. Naming

A MoQ track is a **namespace tuple** plus a **name**. There are no wildcards; the only pattern
mechanism is **namespace-prefix matching**, so fields that need wildcarding go last:

```
namespace = ( meeting, kind,  source, participant )    name = rendition
            ( m-7f3a , audio, main  , p12 )          /  opus
            ( m-7f3a , video, main  , p12 )          /  1080p | 360p | 180p
            ( m-7f3a , video, screen, p12 )          /  1080p
            ( m-7f3a , ctl  , report, p12 )          /  0
            ( m-7f3a , ctl  , plan )                 /  0
```

`meeting` is an opaque value derived from the meeting secret and `participant` a short node id;
nothing human-readable is on the wire. After subscription a track is a numeric alias. A standard
catalog track lists each participant's available tracks.

## 5. Control plane

### Reports in, plan out — as tracks

- **Report** (per node, signed, sequenced): identity, name, capabilities, power/thermal/CPU
  headroom, uplink estimate, metered flag, NAT class, candidate addresses, links held with path kind
  (direct or iroh-relay) / RTT / loss / utilisation, peers it failed to reach, tracks it carries, last plan generation *applied*.
- **Plan** (conductor, signed, stamped (term, generation)): roster; khef peering graph; video
  routing tables; audio overlay and N; priority classes; budgets, caps and per-subscriber limits;
  entitlements; contingency next-hops; conductor successor list.
- **Dissemination**: the `(m, ctl)` prefix has one hard-wired rule needing no plan — forward to
  every neighbour, dedupe by (track, group, object). The conductor needs a path to each node, not
  a connection. Plans go as deltas with full snapshots on join and on mismatch.
- **Everyone holds the whole plan; transit khefs and successors hold all reports.**
- No gossip library: one conductor means nothing to agree on, and 50 nodes are few enough to
  flood. Multiple conductors (later) would each own a disjoint set of publishers; shared state
  (budgets, peering graph, audio overlay, roster) would need partitioning or a coordinating layer.

Also on the control namespace: moderation commands, hole-punch coordination, later key
distribution.

### Links and joining

- **Any khef accepts a connection from anyone who passes admission**; such a link always carries
  control.
- **Media flows on a link only where the plan says so.**

Join: (1) rendezvous finds a member — the first node creates the meeting and is conductor;
(2) connect and run the PAKE; (3) open MoQ, read plan and roster from `(m, ctl)`, publish own
report; (4) the report floods, which is how the conductor learns the node exists; (5) the
conductor issues a plan with the roster entry, default routes and planned peerings; (6) peerings
form, hole punching coordinated over the entry link; (7) the entry link is kept if planned,
otherwise closed once the planned peerings are up.

### Conductor leave, crash, partition

- **Leave**: publish a final plan naming the successor with `term + 1` (the successor already
  holds full state), then drain as a transit khef if it is one. UI: "rebalancing meeting, then
  exiting". Hard cap **3 s**, after which the node exits and is treated as a crash.
- **Crash**: successors watch the conductor's report heartbeat; the first live successor claims
  `term + 1` (lower-ranked successors wait longer). No media impact; only topology changes wait
  (target 1–2 s).
- **Partition**: each connected component runs with its own conductor; on heal the lower
  (term, id) side adopts the other plan.

## 6. Topology

- **Inputs**: the link graph from reports (who is connected, who cannot connect, RTT, loss,
  capacity). The conductor may ask nodes to attempt new links.
- **Transit eligibility and score**: reachable by enough peers, uplink headroom, not battery-only
  / metered / thermally limited unless nothing better exists; scored on reachability, uplink,
  CPU headroom, RTT centrality and session age, with hysteresis.
- **Link preference** for media: (1) a direct link, chosen among by RTT and loss; (2) through a
  transit khef where both hops are direct; (3) an iroh-relay path, only when a node has no
  direct link to any khef. Starting rule: 2 always beats 3; to be refined (latency, avoiding
  hot spots on transit khefs). Control tracks run over any link. A link reported as iroh-relay
  is treated as provisional for a second or two, since most upgrade to direct.
- **Shape**: every edge khef peers with **two or more** khefs where the graph allows. The
  transit graph is connected; a full mesh is preferred, not required. Two nodes may peer directly.
- **Capacity (estimate, to be measured)**: uplink is the binding constraint. A subscriber taking
  5 video tracks plus audio needs very roughly 2–4 Mbps, so a 20 Mbps residential uplink serves
  5–8 subscribers; a 50-person meeting needs on the order of 6–10 transit khefs on residential
  links. Simulcast and top-N are required.
- **Mixing** is never done by a khef. A mixer, recorder, transcriber or AI participant is an SDK
  client that subscribes and publishes like anyone else.

## 7. Video: subscribe by name, routed from a table

1. A client sends `SUBSCRIBE (m, video, main, p12)/360p` to its khef.
2. The khef resolves it from the plan's **routing table**: longest-prefix match over the full
   track name → next-hop khef(s). If it already carries the track it attaches the subscriber;
   otherwise it subscribes upstream, hop by hop, until the publisher's edge khef, which
   subscribes to its local client. A track is produced only while someone subscribes.
3. One group of pictures per QUIC stream; a subscriber joins at the next group start. Keyframes
   are requested with `NEW_GROUP_REQUEST`, forwarded upstream and deduplicated per hop.

```
(m, video, main, p12)            → via khef B          default route for p12's camera
(m, video, main, p12) / 1080p    → via khef D          finer entry where the conductor wants it
(m, video, screen)               → via khef B, C       all screen shares, two next hops
```

- **Default per-participant routes** are installed at join, so media flows on subscribe with no
  conductor action. Finer entries give per-track control where needed. Tables are per khef.
- An entry may carry a **cap** ("nothing above 360p via this hop").
- **Join race**: a roster entry and its routes travel in the same plan generation. A khef with
  no route yet for a subscribe **holds it pending** (bounded) rather than refusing.
- Entitlements and per-subscriber limits in the plan bound what a client may ask for.

## 8. Audio: top-N over a pre-wired overlay

- A client sends one `SUBSCRIBE_TRACKS` for prefix `(m, audio, main)` with a **top-N filter** on
  an `audio_level` object property. Its khef publishes the currently selected tracks to it and
  starts/ends tracks as the selection changes; the client learns who is speaking from the names.
- **Top-N is our extension**, modelled on the individual "MOCHA meetings" draft; the transport
  draft has only range filters.
- **Between khefs** the plan pre-wires one audio overlay: a loop-free tree that reaches every
  node, as standing top-N subscriptions. Top-N runs only on overlay links, not on every link a
  khef holds. (A second tree for crash failover is deferred; Opus DRED may cover loss.)
- **Every audio track is announced along the whole overlay at join**, so aliases and forwarding
  state exist at every khef before anyone speaks. Top-N gates only the data. This is what keeps
  the first syllable of a new speaker.
- **Publishers always send** whatever the Opus encoder emits: speech packets while talking, and
  either DTX packets (a few per second) or continuous low-level packets while silent. Top-N
  does not depend on DTX, only on something being sent with a level; we do not pre-process the
  signal to force DTX. (In spike 5 DTX did not engage on one real microphone's noise floor; to
  watch.) Each packet carries `audio_level`, computed on that frame with no smoothing lag.
  It is object property `0x78` (in the draft's application range, one byte on the wire) with an
  integer value from 0 to 127; a typical audio datagram header is 7 bytes.
  Opus in-band FEC and DRED are planned from the start. One object per group, in datagrams.
- **Selection rule** (per khef, separately for each overlay link and for its own client; the
  candidates for a link are the tracks not learned from that link). The khef holds a selected
  set of N tracks, each with the time it last ranked in. On each packet arrival, with every
  track's level taken from its latest packet:
  1. Rank the candidates by level. Each selected track among the N loudest *and* above the
     minimum level has its time refreshed.
  2. A track among the N loudest that is not selected, and is above the minimum level, replaces
     the quietest selected track whose time is older than the **hold** (ties: the oldest). If no
     selected track is that old, it is not added.
  - **Fast in, slow out**: entry happens on the first speech packet; a track leaves only when
    someone else needs the slot and it has not ranked in for the hold time. Slots are never
    emptied, so when everyone is silent the set does not change, and quiet background noise is
    consistently in or out.
  - No timers: the rule runs on packet arrival, so it adds no delay.
  - **Level decay**: a track's level is held for two packet intervals after its latest packet,
    then decays linearly to zero at 100 ms, so a crashed talker or a lost end-of-speech packet
    cannot hold a slot. Computed on arrival of any packet; still no timers.
  - Hold time (250 ms, from spike 4) and minimum level are profile constants.
  - Known gap: a word whose first frame is below the minimum level loses that 20 ms frame.
    Possible fix: khefs keep the latest packet of each unselected track and send it first.
- **Consistency is eventual.** Khefs nearer a new speaker switch first; the change crosses the
  overlay at forwarding speed. With more than N simultaneous talkers, listeners may hear
  different subsets. No overlay link carries more than N tracks per direction. A leaf khef
  always forwards its own client; a transit khef ranks its client against its other links.
- Declared levels are untrusted and bounded by conductor priority rules (host/pinned always
  pass; server-muted never pass).

## 9. Leave and crash

- **Transit khef leaves or resigns** (no media impact): it announces draining (`GOAWAY` with a
  timeout); the conductor issues new routes; each downstream khef subscribes via the new next
  hop while still receiving from the old, dedupes by (track, group, object), and drops the old
  path once the new one is live; the conductor then releases the leaver. Clients see nothing.
- **Transit khef crashes** (minimal impact): detected in roughly 200–500 ms (audio flows every
  20 ms; QUIC probe). Downstream khefs apply the plan's **contingency next-hops** without
  waiting for the conductor, which then issues a clean plan. Audio gaps until the contingency hop takes over (DRED conceals part of it); a video
  track resumes at the next group from the alternate path.
- **A node leaves or crashes**: its client and edge khef go together and its tracks end.
- **Conductor**: §5.

## 10. Congestion and rate control

Baseline, to be refined by a dedicated **rate-control spike** (plan phase 3→4):

- **Khefs shed by MoQ priority** on a congested link and reset groups past their delivery
  timeout.
- **Priorities on khef links**: conductor-set classes in the plan, matched by longest prefix
  (e.g. audio > screen share > low renditions > high renditions), used as the subscriber priority
  of a khef's upstream subscription. Publisher priority passes through and breaks ties. A
  receiver's own priorities apply on its last hop.
- **Marking**: a khef sets a hop-local "congestion experienced" property on objects forwarded
  over a congested link and reports the link's utilisation and loss.
- **Mid-path congestion** (e.g. between the second and third khef of a chain): the middle khef
  sheds and marks; the receiving side moves to a lower rendition, which propagates an unsubscribe
  and eases the link; the conductor reroutes (per-track entries) or publishes a cap.
- **Uplink congestion**: the publisher's khef sees it first-hand, in-process with the encoder.
  It lowers bitrates, then stops producing the rendition that frees the most uplink for the least
  loss (never the lowest, never audio), ends that track and marks it unavailable in the catalog.
- **Who switches renditions.** The default is the receiving side: the receiver's edge khef
  automates switching so applications get it for free. **Swapping one track for another under the
  hood inside the khef network is not ruled out**; how that would work (and how it interacts
  with authenticated track names) is the main question for the rate-control spike.

## 11. Remote clients

A client speaks MoQT to *a* khef; it need not be the local one.

- **Thin Palaver clients** (SDK integrations, phones that should not run a khef, a web client we
  write): connect to a reachable member, run the PAKE, open MoQ, read the plan, publish a report
  marked "no khef", then publish and subscribe through that khef. Ordinary participants: same
  password, same encryption.
- **Browsers** cannot be khefs (no inbound connections, no UDP sockets; WebTransport is
  client-only), so a web client is always a thin client of a native khef. It needs WebTransport
  and a certificate it trusts: a khef's identity is self-generated by default but can be
  configured with a CA-signed certificate (public CA, or an organisation's own CA).
- **Third-party MoQ clients (future, not planned)**: a member generates a MoQ URL (khef address
  plus an unguessable, revocable, scoped secret); the entry khef acts as a proxy participant —
  publishes a report on the client's behalf, enforces scope, and, once frame encryption exists,
  decrypts and encrypts for it. Deferred questions: default scope, who may generate a URL,
  certificate configuration.

## 12. Our khef: scope

A **profile** of MoQT, deliberately small:

- **In**: session setup; `SUBSCRIBE`, `PUBLISH`, `SUBSCRIBE_TRACKS` / `SUBSCRIBE_NAMESPACE`,
  `REQUEST_UPDATE`; subgroup streams and datagrams; priorities and delivery timeouts;
  `NEW_GROUP_REQUEST`; `GOAWAY`; immutable and mutable properties; property range filters.
- **Our extensions** (khef↔khef, and offered to our own clients): top-N filter; congestion
  mark; per-link loss sequence; the control-namespace flood rule; plan semantics.
- **Out initially**: `FETCH` and caching beyond the current group (except the plan track); pause.
- **Draft**: pinned to **draft 22**. Khef logic is ours, and so is the codec: `moq-wire`
  implements only the messages in this profile (spike 4). The draft closes a session on an
  unknown request parameter, and no existing Rust codec lets an application register one, so
  top-N cannot go through them. `moqtap-codec` is a dev-only dependency for
  cross-checking encodings in tests (it brings 8 packages; MOQtail was dropped as a second
  oracle because its transport brings about 160, and can be added temporarily to break a tie).
- **Extension negotiation**: every khef and Palaver client sends a Palaver setup option in
  `SETUP` (unknown setup options are ignored by other implementations). Our request parameters
  (top-N) are used only on connections where both sides sent it; a peer that did not is treated
  as a plain draft 22 endpoint. The option is type `0x50414C57` (odd, so it carries bytes; not a
  grease value) holding a version and the sender's audio top-N; fields added by later versions
  are ignored by older ones, and the lower of the two versions is the one in force. The number
  is self-assigned: the draft's setup option registry has no private-use range and asks for a
  provisional registration, which we should request before interoperating with others.
- **Transport**: iroh; always QUIC end to end, over a direct UDP path or an iroh-relay path.
  Introduction is done by members over the control tracks, with iroh-relay as the helper and
  fallback (spike 1); quinn as fallback; behind a trait that offers request streams, unidirectional
  streams and datagrams.
- **Rules from the khef spike**: group ids count for the life of a track; hold data streams whose
  alias is not known yet; linger before unsubscribing upstream; release an old upstream only once
  the new one delivers a group start; bounded dedupe window.

## 13. Security, phased

- **Phase A**: QUIC/TLS per hop between PAKE-authenticated members. Payloads are opaque to khefs
  from day one.
- **Phase B**: frame encryption using MoQ's secure-objects draft (SFrame-derived; authenticates
  track name, group/object ids and immutable properties), with a meeting key the conductor
  generates, distributes on the control namespace and rotates on leave/kick. Needed before any
  non-member MoQ relay (a public relay network) carries media. An iroh-relay only forwards
  QUIC packets encrypted between khefs, so it does not need this. Sender signatures
  for attribution are not part of secure-objects and would be our addition if wanted.

## 14. Media pipeline

No libwebrtc. The pipeline is assembled from parts behind our own traits in `media` (spike 5):

- **Audio processing**: `sonora`, a pure-Rust port of WebRTC's echo canceller, noise suppression
  and gain control. Platform voice-processing units on Apple/Android where better.
- **Audio codec**: libopus 1.6 (C, built from source) with DTX, in-band FEC and DRED.
- **Audio I/O**: `cpal` (WASAPI, ALSA or PulseAudio, CoreAudio). Devices differ in rate and
  sample format, so the pipeline resamples to 48 kHz float.
- **Jitter buffer**: our own. Adaptive playout delay and time-stretching, with concealment handed
  to Opus (FEC, DRED, neural concealment). The `neteq` crate is reference material.
- **Video codec**: our own encoder/decoder interface. openh264 (software) is the first backend;
  **hardware backends follow early (phase 2)**, because a node must keep CPU for its khef.
  Media Foundation on Windows first, then VideoToolbox, VAAPI/NVENC, MediaCodec. `moq-video`'s
  backends (MIT/Apache) are the starting point to copy from, without its `moq-net`/`hang`
  dependencies. Software encoders may skip frames to hold a bitrate; the pipeline tolerates it.
- **Video capture**: not yet examined (`nokhwa`, `scap`, or `moq-video`'s capture code).
- Simulcast as separate encoder sessions, one track per rendition. Media container: MoQ's
  low-overhead container (LOC).
- **Distribution notes**: unsigned Windows builds are blocked by Smart App Control, so code
  signing is required; H.264 patent cover for a source-built openh264 needs checking (Cisco's
  cover is understood to apply to its own binaries only).

## 15. SDK and headless clients

The `sdk` crate is the only way anything talks to a meeting: join/leave, roster and role events,
mute, subscribe/publish by track name, stats; media in and out as raw frames or encoded objects;
options to run without devices, without transit duty, or as khef-only. Rust first, then UniFFI
(Python first). `apps/bot` is the reference headless client: synthetic or file media, scripted
behaviour, metrics and event log; it serves tests, manual dev sessions, load, the reachable-khef
role, and as the skeleton for recorder/transcriber/AI participants.

## 16. Operability without servers

Local structured event log and metrics ring per node; every plan logged with generation and
inputs (every node holds the plan, so one log explains the meeting). In-call stats overlay per
track: path, bitrate, loss by hop, jitter, RTT. Meeting health view from reports. One-click
redacted diagnostics bundle. Optional OTLP sink, off by default. Version negotiated at admission;
the conductor picks a meeting-wide version; too-old clients are refused clearly. No participant
or meeting identifiers as metric labels.

## 17. Testing

See [04-dev-environment-and-flow.md](04-dev-environment-and-flow.md): unit and guard tests,
deterministic simulation (the workhorse: routing, top-N convergence, migration, crash, conductor
hand-over, non-full-mesh graphs), local end-to-end with N bots, emulated networks, distributed
runs, native media. The simulator is our own (virtual clock, seeded randomness, ordered event
queue, link and fault model, invariants checked as it runs); see `docs/spikes/05-simulator.md`.

## 18. Risks

1. **Rate adaptation** across multi-hop khefs; least proven part. Dedicated spike.
2. **First-contact NAT traversal** without an operated server (iroh-relay is the fallback).
3. **MoQ draft churn** (five drafts in six months; RFC unlikely before mid/late 2027). Contained
   by `moq-wire` and by conformance being an edge property.
4. **Uplink limits** on residential transit khefs at 25–50 participants.
5. **Plan quality** on awkward link graphs; flooded control correctness at 50 nodes.
6. **Media parts are young**: `sonora` and the hardware codec backends are recent code; our own
   jitter buffer is unproven until tested with real loss and jitter.
7. **Non-standard pieces**: top-N, congestion mark; secure-objects still at -01.
8. **Low-entropy passwords** vs offline guessing on DHT records; default to generated links.
9. **Patents** (Topia mixed mesh/forwarding; Skype-era supernode).

## 19. Decisions

| Topic | Decision |
|---|---|
| Name / licence | Palaver; MIT OR Apache-2.0; no GPL or static-LGPL dependencies |
| Scale | 25–50 participants, single conductor; multi-khef from the start |
| Architecture | MoQ-based: edge khef per node, named tracks, routing tables, control as tracks |
| Subscribe resolution | Routing table, longest-prefix match; default per-participant routes; conductor not in the path; unroutable subscribes held pending |
| Audio | Top-N selected by khefs over one pre-wired loop-free overlay; loudness-ranked, fast in, slow out; all audio tracks announced at join; DTX, FEC and DRED |
| Rendition switching | Receiver-side by default; khef-side track swap allowed if the rate-control spike finds a clean way |
| Local client↔khef | In-memory MoQ-formatted messages; no sockets, QUIC or crypto |
| Network edge | Conformant to the pinned draft for our profile |
| Draft / codec | Draft 22; our own codec in `moq-wire` for our profile; `moqtap-codec` as the test oracle |
| Transport | iroh; members introduce, iroh-relay as helper and last-resort path; quinn as fallback |
| Link preference | Direct, then via a transit khef, then iroh-relay path (media only if nothing else) |
| Naming | **khef** = the MoQ relay in each node; **iroh-relay** = iroh's connection helper |
| Infrastructure | Public STUN-like services fine; iroh-relay as last resort; public MoQ relay network later |
| Identity / roles | User-provided names; host = creator, then longest-present |
| Remote clients | Thin Palaver clients yes (same password); third-party MoQ URL is future |
| Leave | Hard cap 3 s |
| dark_tower | Separate project; copy code, share nothing |
| UI shell | Deferred; clean, professional, strong media support |
| Media | No libwebrtc: `sonora` + libopus 1.6 (DRED) + `cpal`; own jitter buffer; own video interface with openh264 first and hardware backends in phase 2 |

## 20. Open questions

1. Rate control: receiver-side switching vs khef-side track swap; fairness on a shared congested
   hop; which rendition a publisher drops (spike).
2. What changed between MoQT drafts 22 and 23, and when to move.
3. How the PAKE is carried for a web client (before MoQ on the same connection is the default).
