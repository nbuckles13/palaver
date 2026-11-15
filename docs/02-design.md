# Palaver — design

Status: draft 3, 2026-11-15. The MoQ-based design, confirmed. It merges the two earlier drafts
kept in `archive/` (the original slot-based design and the MoQ-native alternative).

Terms: a **node** is one running instance (app or headless). A node contains a **client**
(capture, encode, decode, render) and a **relay** (a small MoQ relay of our own). Every relay is an
**edge relay** for its own client; the conductor also uses some as **transit relays** ("hubs").
One node at a time holds the **conductor** role. MoQT = Media over QUIC Transport; facts about it
are from draft-ietf-moq-transport-23 unless noted (see [archive/moq-evaluation.md](archive/moq-evaluation.md)).

Scope: **25–50 participants**, single conductor.

## 1. Principles

1. **Conductor decides, relays execute.** The conductor is never in a hot path: not media, not
   subscription fulfilment, not audio selection, not crash reaction. Relays keep working on their
   last applied plan if it vanishes.
2. **One protocol.** Clients talk MoQT to a relay; relays talk MoQT to each other; control state
   rides MoQ tracks. MoQT treats a coordinated set of relays as one relay and leaves its insides
   unspecified, so routing, selection and the conductor are ours.
3. **A client has one relay for life.** Multi-path, migration and failover happen between relays.
   Clients never migrate.
4. **No single-relay special case.** The general multi-relay algorithm always runs.
5. **Connected, not fully meshed.** Nothing assumes a direct link between two particular nodes,
   including to the conductor.
6. **Relays are media-blind.** They act on names, priorities, properties and group boundaries,
   never on codec or media kind. A track switches source or path only at a group start (video: a
   keyframe; audio: every object).
7. **Pure decision functions.** Relay behaviour and plan computation are sans-IO.
8. **The engine is an SDK.** GUI app, bots, recorders, transcribers and AI participants are all
   consumers of the same API.
9. **MoQ conformance is an edge property.** The network client↔relay edge follows the pinned
   draft for our profile. Relay↔relay links are ours and versioned.

## 2. Node anatomy

```
┌──────────────────────────── node (one process) ────────────────────────────┐
│  app: GUI shell │ headless bot │ recorder │ custom integration             │
│  ───────────── SDK (events out, commands in, raw/encoded media in/out) ─── │
│  client: capture → audio processing → encode ─┐   render ← decode ← jitter │
│                                               │ MoQT, in-memory (no QUIC)  │
│  relay: subscriptions · routing table · top-N · priorities · dedupe        │
│        │ MoQT over QUIC                                                    │
│        └──────────────► other nodes' relays                                │
│  control: reports out, plan in; conductor role if elected                  │
│  net: QUIC endpoint, hole punching, rendezvous, admission (PAKE)           │
│  ops: metrics ring, structured event log, diagnostics bundle               │
└────────────────────────────────────────────────────────────────────────────┘
```

**The local hop is not a network hop.** Client and relay exchange MoQ-formatted control messages
and reference-counted object buffers over an in-memory channel: no sockets, no QUIC, no TLS,
nothing sent or encrypted twice.

Crates (Rust workspace, permissive licence; code copied from dark_tower where useful, nothing
shared):

| Crate | Contents | IO? |
|---|---|---|
| `moq-wire` | MoQT message and object encoding for the pinned draft, isolated so a draft change touches only this crate (may wrap an external crate) | none |
| `proto` | Our control payloads: report, plan, roster, commands; naming scheme; test vectors | none |
| `relay-core` | Relay state machine: sessions, subscriptions, routing, top-N, priorities, dedupe, congestion marking | none |
| `topology` | (roster, reports, link graph) → plan | none |
| `control` | Membership, conductor election and hand-over | sans-IO core + driver |
| `net` | QUIC endpoint, in-memory transport, rendezvous, admission handshake | yes |
| `media` | Capture/encode/decode/render/audio-processing/jitter traits, platform impls, synthetic sources | yes |
| `sdk` | Public API; later UniFFI bindings | yes |
| `sim` | Deterministic multi-node simulator | none |
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
  members as introducers. A node that cannot punch attaches through any relay it can reach; one
  that can reach nobody uses a TURN-like relay (a public service, or a headless Palaver node on a
  reachable machine).
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
  headroom, uplink estimate, metered flag, NAT class, candidate addresses, links held with RTT /
  loss / utilisation, peers it failed to reach, tracks it carries, last plan generation *applied*.
- **Plan** (conductor, signed, stamped (term, generation)): roster; relay peering graph; video
  routing tables; audio overlay and N; priority classes; budgets, caps and per-subscriber limits;
  entitlements; contingency next-hops; conductor successor list.
- **Dissemination**: the `(m, ctl)` prefix has one hard-wired rule needing no plan — forward to
  every neighbour, dedupe by (track, group, object). The conductor needs a path to each node, not
  a connection. Plans go as deltas with full snapshots on join and on mismatch.
- **Everyone holds the whole plan; transit relays and successors hold all reports.**
- No gossip library: one conductor means nothing to agree on, and 50 nodes are few enough to
  flood. Multiple conductors (later) would each own a disjoint set of publishers; shared state
  (budgets, peering graph, audio overlay, roster) would need partitioning or a coordinating layer.

Also on the control namespace: moderation commands, hole-punch coordination, later key
distribution.

### Links and joining

- **Any relay accepts a connection from anyone who passes admission**; such a link always carries
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
  holds full state), then drain as a transit relay if it is one. UI: "rebalancing meeting, then
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
- **Shape**: every edge relay peers with **two or more** relays where the graph allows. The
  transit graph is connected; a full mesh is preferred, not required. Two nodes may peer directly.
- **Capacity (estimate, to be measured)**: uplink is the binding constraint. A subscriber taking
  5 video tracks plus audio needs very roughly 2–4 Mbps, so a 20 Mbps residential uplink serves
  5–8 subscribers; a 50-person meeting needs on the order of 6–10 transit relays on residential
  links. Simulcast and top-N are required.
- **Mixing** is never done by a relay. A mixer, recorder, transcriber or AI participant is an SDK
  client that subscribes and publishes like anyone else.

## 7. Video: subscribe by name, routed from a table

1. A client sends `SUBSCRIBE (m, video, main, p12)/360p` to its relay.
2. The relay resolves it from the plan's **routing table**: longest-prefix match over the full
   track name → next-hop relay(s). If it already carries the track it attaches the subscriber;
   otherwise it subscribes upstream, hop by hop, until the publisher's edge relay, which
   subscribes to its local client. A track is produced only while someone subscribes.
3. One group of pictures per QUIC stream; a subscriber joins at the next group start. Keyframes
   are requested with `NEW_GROUP_REQUEST`, forwarded upstream and deduplicated per hop.

```
(m, video, main, p12)            → via relay B          default route for p12's camera
(m, video, main, p12) / 1080p    → via relay D          finer entry where the conductor wants it
(m, video, screen)               → via relay B, C       all screen shares, two next hops
```

- **Default per-participant routes** are installed at join, so media flows on subscribe with no
  conductor action. Finer entries give per-track control where needed. Tables are per relay.
- An entry may carry a **cap** ("nothing above 360p via this hop").
- **Join race**: a roster entry and its routes travel in the same plan generation. A relay with
  no route yet for a subscribe **holds it pending** (bounded) rather than refusing.
- Entitlements and per-subscriber limits in the plan bound what a client may ask for.

## 8. Audio: top-N over a pre-wired overlay

- A client sends one `SUBSCRIBE_TRACKS` for prefix `(m, audio, main)` with a **top-N filter** on
  an `audio_level` object property. Its relay publishes the currently selected tracks to it and
  starts/ends tracks as the selection changes; the client learns who is speaking from the names.
- **Top-N is our extension**, modelled on the individual "MOCHA meetings" draft; the transport
  draft has only range filters.
- **Between relays** the plan pre-wires an audio overlay (a loop-free tree, or two for
  redundancy) as standing top-N subscriptions. Each relay ranks every audio track it sees and
  forwards on each overlay link the top N it did not learn from that link. A meeting-wide top-N
  track is top-N at every relay that sees it, so it reaches everyone; anything else stops where
  it loses; no link carries more than N.
- Audio is one object per group, in datagrams, sent over two overlay paths by default; receivers'
  relays dedupe. Silent or muted clients send nothing. Declared levels are untrusted and bounded
  by conductor priority rules (host/pinned always pass; server-muted never pass).

## 9. Leave and crash

- **Transit relay leaves or resigns** (no media impact): it announces draining (`GOAWAY` with a
  timeout); the conductor issues new routes; each downstream relay subscribes via the new next
  hop while still receiving from the old, dedupes by (track, group, object), and drops the old
  path once the new one is live; the conductor then releases the leaver. Clients see nothing.
- **Transit relay crashes** (minimal impact): detected in roughly 200–500 ms (audio flows every
  20 ms; QUIC probe). Downstream relays apply the plan's **contingency next-hops** without
  waiting for the conductor, which then issues a clean plan. Audio has no gap (two paths); a video
  track resumes at the next group from the alternate path.
- **A node leaves or crashes**: its client and edge relay go together and its tracks end.
- **Conductor**: §5.

## 10. Congestion and rate control

Baseline, to be refined by a dedicated **rate-control spike** (plan phase 3→4):

- **Relays shed by MoQ priority** on a congested link and reset groups past their delivery
  timeout.
- **Priorities on relay links**: conductor-set classes in the plan, matched by longest prefix
  (e.g. audio > screen share > low renditions > high renditions), used as the subscriber priority
  of a relay's upstream subscription. Publisher priority passes through and breaks ties. A
  receiver's own priorities apply on its last hop.
- **Marking**: a relay sets a hop-local "congestion experienced" property on objects forwarded
  over a congested link and reports the link's utilisation and loss.
- **Mid-path congestion** (e.g. between the second and third relay of a chain): the middle relay
  sheds and marks; the receiving side moves to a lower rendition, which propagates an unsubscribe
  and eases the link; the conductor reroutes (per-track entries) or publishes a cap.
- **Uplink congestion**: the publisher's relay sees it first-hand, in-process with the encoder.
  It lowers bitrates, then stops producing the rendition that frees the most uplink for the least
  loss (never the lowest, never audio), ends that track and marks it unavailable in the catalog.
- **Who switches renditions.** The default is the receiving side: the receiver's edge relay
  automates switching so applications get it for free. **Swapping one track for another under the
  hood inside the relay network is not ruled out**; how that would work (and how it interacts
  with authenticated track names) is the main question for the rate-control spike.

## 11. Remote clients

A client speaks MoQT to *a* relay; it need not be the local one.

- **Thin Palaver clients** (SDK integrations, phones that should not run a relay, a web client we
  write): connect to a reachable member, run the PAKE, open MoQ, read the plan, publish a report
  marked "no relay", then publish and subscribe through that relay. Ordinary participants: same
  password, same encryption.
- **Browsers** cannot be relays (no inbound connections, no UDP sockets; WebTransport is
  client-only), so a web client is always a thin client of a native relay. It needs WebTransport
  and a certificate it trusts: a relay's identity is self-generated by default but can be
  configured with a CA-signed certificate (public CA, or an organisation's own CA).
- **Third-party MoQ clients (future, not planned)**: a member generates a MoQ URL (relay address
  plus an unguessable, revocable, scoped secret); the entry relay acts as a proxy participant —
  publishes a report on the client's behalf, enforces scope, and, once frame encryption exists,
  decrypts and encrypts for it. Deferred questions: default scope, who may generate a URL,
  certificate configuration.

## 12. Our relay: scope

A **profile** of MoQT, deliberately small:

- **In**: session setup; `SUBSCRIBE`, `PUBLISH`, `SUBSCRIBE_TRACKS` / `SUBSCRIBE_NAMESPACE`,
  `REQUEST_UPDATE`; subgroup streams and datagrams; priorities and delivery timeouts;
  `NEW_GROUP_REQUEST`; `GOAWAY`; immutable and mutable properties; property range filters.
- **Our extensions** (relay↔relay, and offered to our own clients): top-N filter; congestion
  mark; per-link loss sequence; the control-namespace flood rule; plan semantics.
- **Out initially**: `FETCH` and caching beyond the current group (except the plan track); pause.
- **Draft**: pinned to the latest, or to whatever a usable crate supports. Relay logic is ours;
  the wire codec may come from a crate if one separates cleanly from transport and routing
  (candidates: MOQtail, the codec layer of `moq-net`).
- **Transport**: iroh 1.0 preferred, quinn as fallback, behind a trait.

## 13. Security, phased

- **Phase A**: QUIC/TLS per hop between PAKE-authenticated members. Payloads are opaque to relays
  from day one.
- **Phase B**: frame encryption using MoQ's secure-objects draft (SFrame-derived; authenticates
  track name, group/object ids and immutable properties), with a meeting key the conductor
  generates, distributes on the control namespace and rotates on leave/kick. Needed before any
  non-member relay (public relay network, TURN-like service) carries media. Sender signatures
  for attribution are not part of secure-objects and would be our addition if wanted.

## 14. Media pipeline

libwebrtc where it is the best tool, via the LiveKit Rust crates: audio processing (echo
cancellation, noise suppression, gain) from the start, with platform voice-processing units on
Apple/Android where better; its jitter buffer, capture and codecs if usable outside its own
transport (spike). Otherwise `cpal`/`nokhwa`/`scap`, Opus, platform H.264 with a software
fallback. Simulcast as separate encoder sessions, one track per rendition. Media container: MoQ's
low-overhead container (LOC).

## 15. SDK and headless clients

The `sdk` crate is the only way anything talks to a meeting: join/leave, roster and role events,
mute, subscribe/publish by track name, stats; media in and out as raw frames or encoded objects;
options to run without devices, without transit duty, or as relay-only. Rust first, then UniFFI
(Python first). `apps/bot` is the reference headless client: synthetic or file media, scripted
behaviour, metrics and event log; it serves tests, manual dev sessions, load, the reachable-relay
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
runs, native media.

## 18. Risks

1. **Rate adaptation** across multi-hop relays; least proven part. Dedicated spike.
2. **First-contact NAT traversal** without an operated relay.
3. **MoQ draft churn** (five drafts in six months; RFC unlikely before mid/late 2027). Contained
   by `moq-wire` and by conformance being an edge property.
4. **Uplink limits** on residential transit relays at 25–50 participants.
5. **Plan quality** on awkward link graphs; flooded control correctness at 50 nodes.
6. **libwebrtc à la carte**: how much is usable outside its own transport is unverified.
7. **Non-standard pieces**: top-N, congestion mark; secure-objects still at -01.
8. **Low-entropy passwords** vs offline guessing on DHT records; default to generated links.
9. **Patents** (Topia mixed mesh/forwarding; Skype-era supernode).

## 19. Decisions

| Topic | Decision |
|---|---|
| Name / licence | Palaver; open source, permissive; no GPL or static-LGPL dependencies |
| Scale | 25–50 participants, single conductor; multi-relay from the start |
| Architecture | MoQ-based: edge relay per node, named tracks, routing tables, control as tracks |
| Subscribe resolution | Routing table, longest-prefix match; default per-participant routes; conductor not in the path; unroutable subscribes held pending |
| Audio | Top-N selected by relays over a pre-wired overlay; two paths by default |
| Rendition switching | Receiver-side by default; relay-side track swap allowed if the rate-control spike finds a clean way |
| Local client↔relay | In-memory MoQ-formatted messages; no sockets, QUIC or crypto |
| Network edge | Conformant to the pinned draft for our profile |
| Draft | Latest, or what the best usable crate supports |
| Infrastructure | Public STUN-like services fine; TURN-like relay as last resort; public MoQ relay network later |
| Identity / roles | User-provided names; host = creator, then longest-present |
| Remote clients | Thin Palaver clients yes (same password); third-party MoQ URL is future |
| Leave | Hard cap 3 s |
| dark_tower | Separate project; copy code, share nothing |
| UI shell | Deferred; clean, professional, strong media support |
| libwebrtc | Use where it makes sense |

## 20. Open questions

1. Rate control: receiver-side switching vs relay-side track swap; fairness on a shared congested
   hop; which rendition a publisher drops (spike).
2. Is there a crate fit to be the wire codec (spike)?
3. How the PAKE is carried for a web client (before MoQ on the same connection is the default).
