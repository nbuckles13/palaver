# Palaver — high-level design (sketch)

Status: draft 2, incorporating review comments of 2026-11-14. Terms: a **node** is one running
instance (app or headless). Each node contains a **client** (capture, encode, decode, render) and a
**hub** (the embedded SFU-style forwarder). One node at a time also holds the **conductor** role
(control-plane decisions). Roughly: hub ≈ dark_tower MH, conductor ≈ MC; there is no AC or GC.

Scope for now: **25–50 participants**, single conductor. Larger meetings would revisit the
single-conductor choice.

## 1. Principles

1. **Conductor decides, hubs execute, clients obey.** Hubs never query the conductor in the hot path
   and keep forwarding on their last applied plan if it vanishes.
2. **A client cannot tell a local hub from a remote one.** One protocol, two transports: QUIC for
   remote, an in-process channel for local.
3. **No active/standby.** A client holds connections to several hubs. The conductor directs, per
   stream, which hubs the client sends to and, per slot, which hub it receives from. A client may be
   sending and receiving via several hubs at once. Redundancy is a property of the plan, not of a
   connection role.
4. **No single-hub special case.** The topology computation always runs the general multi-hub
   algorithm; one hub is N=1. Multi-hub is in from the start.
5. **Connected, not fully meshed.** The design requires only that the meeting graph is connected.
   No component may assume a direct connection between two particular nodes — including between a
   node and the conductor.
6. **Hubs are type-blind.** They forward by header flags and plan, never by media kind or codec.
   The one piece of media structure they see is a generic **switch-point flag**
   ("independently decodable"): a hub changes the source of an output only on a flagged frame. Video
   sets it on keyframes; audio sets it on every frame.
7. **Pure decision functions.** Hub forwarding and topology computation are sans-IO functions of
   (state, input) → output.
8. **The engine is an SDK.** The GUI app, headless bots, recorders, transcribers and AI
   participants are all consumers of the same API.

## 2. Node anatomy

```
┌──────────────────────────── node (one process) ────────────────────────────┐
│  app: GUI shell │ headless bot │ recorder │ custom integration             │
│  ───────────── SDK (events out, commands in, raw/encoded media in/out) ─── │
│  client                         │  hub (idle unless the plan uses it)      │
│   capture → APM → encode ─┐     │   ingress → plan table → egress          │
│   render ← decode ← jitter┘     │   (type-blind, per-destination queues)   │
│        │ hub protocol           │        ▲ hub protocol                    │
│        ├── in-proc transport ───┘        │                                 │
│        └── QUIC ─────────────────────────┼──► other nodes' hubs            │
│  control: reports out, plan in, relay of both; conductor role if elected   │
│  net: QUIC endpoint, hole punching, rendezvous (ticket / mDNS / DHT)       │
│  ops: metrics ring, structured event log, diagnostics bundle               │
└────────────────────────────────────────────────────────────────────────────┘
```

Crate layout (Rust workspace, permissive licence, no code shared with dark_tower — code is copied
and adapted where useful):

| Crate | Contents | IO? |
|---|---|---|
| `proto` | Frame header codec, control messages, test vectors | none |
| `hub-core` | Forwarding state machine: plan table, switch-point logic, per-destination queues | none |
| `topology` | (roster, reports, link graph) → plan | none |
| `control` | Membership, report/plan dissemination, conductor election and hand-over | sans-IO core + driver |
| `net` | QUIC endpoint, in-proc transport, rendezvous, PAKE handshake | yes |
| `media` | Capture/encode/decode/render/audio-processing/jitter traits + platform impls + synthetic sources | yes |
| `sdk` | The public API over all of the above; later UniFFI bindings (Python, Swift, Kotlin) | yes |
| `sim` | Deterministic multi-node simulator (virtual clock and network) | none |
| `apps/bot` | Headless client: scriptable, synthetic or file media, metrics endpoint | yes |
| `apps/harness` | egui dev UI | yes |

## 3. Discovery and admission

Inputs: **meeting id** + **password** (or a link encoding both). Names are user-provided and
anonymous for now; org directory / SSO is a later layer on admission.

```
root    = Argon2id(password, salt = meeting_id)
K_topic = HKDF(root, "topic" ‖ time_epoch)   → rendezvous key; encrypts announcements
K_auth  = HKDF(root, "auth")                 → never published; input to a PAKE
```

- **Rendezvous is pluggable**; a node runs every source available and merges results:
  ticket (invite link carries the inviter's key and addresses), mDNS/LAN, mainline DHT under
  `K_topic`, optionally Nostr relays.
- **Reachability** uses public STUN-like address discovery, port mapping (PCP/NAT-PMP/UPnP), IPv6,
  and hole punching. Once a node knows one member, **members act as introducers** for punching to
  further members.
- **When punching fails**, the first answer is the topology itself: the node attaches via any hub
  it *can* reach (principle 5). If it can reach no member at all, a **TURN-like relay** carries its
  connection to a hub. That relay can be a public service or simply **a headless Palaver node run
  on a reachable machine** (an org's VM, someone's home server): it joins as a hub-capable member
  that never publishes media. With frame-level encryption (§7 phase B) a relay that is *not* a
  member sees only ciphertext.
- **Admission**: every new connection runs a balanced PAKE (CPace or SPAKE2) on `K_auth`, bound to
  both nodes' transport keys. Node identity is an Ed25519 key generated per meeting.
- **Join**: connect to any member → PAKE → receive current roster and plan → announce self. The
  entry member relays the join to the conductor over the overlay; no direct conductor connection is
  required.
- **Host role**: the meeting creator (first node) is host; the host role is a roster attribute,
  independent of who is conductor. Host can remote-mute, kick and lock. If the host leaves, the
  role passes to a host-designated co-host, else the longest-present member.

## 4. Control plane

### Shape: link-state in, plan out

Modelled on a link-state routing protocol, with one decider.

- **Reports (in).** Every node periodically issues a signed, sequence-numbered **report**: identity,
  capabilities, power/thermal/CPU headroom, uplink estimate, metered flag, NAT class, the peers it
  holds connections to with RTT and loss, peers it tried and failed to reach, its receive
  capability (slots), and the last plan generation it applied.
- **Plan (out).** The conductor turns roster + reports into a signed **plan** stamped
  (term, generation): which nodes act as hubs; explicit forwarding edges per hub
  ("stream S: from upstream U to downstreams D"); per-client send directives (stream → hubs) and
  slot sources (slot → hub); priority groups; contingency routes (§6); the conductor successor
  list.
- **Dissemination.** Both travel over the existing connection graph by flooding with
  sequence/generation dedupe, plus periodic digest exchange between neighbours to repair gaps. So
  the conductor needs a *path* to each node, not a connection, and other nodes carry control state
  for it exactly as they carry media.
- **Everyone holds the whole plan; hubs and successors hold all reports.** This costs a little
  bandwidth (plans are sent as deltas against a generation, with full snapshots on join and on
  digest mismatch) and buys three things: a hub can validate what a neighbour asks of it; any node
  can explain the meeting from its own log; and a successor conductor starts with full state.
- **Applied-generation echo.** A node's report carries the generation it has *applied*, never the
  one it received. The conductor re-sends on mismatch.

At 50 nodes this is small: tens of reports per second meeting-wide at a few hundred bytes each, and
plan deltas only on structural change.

No gossip library is needed: gossip is a dissemination technique for groups too large to flood, and
it does not provide agreement. With one conductor there is nothing to agree on (the conductor's
signed plan is the truth) and 50 nodes are small enough to flood. Multiple conductors would need a
coordination scheme (partitioned responsibility or real consensus), which is a separate problem
from dissemination. Revisit both above ~100 participants.

### What the conductor is not in the path of

- Media forwarding.
- **Loudest-N audio and active-speaker selection** (below). The conductor sets the rules; hubs
  apply them per frame.
- Crash reaction (§6): nodes act on pre-pushed contingency routes.

### Audio: hubs forward the loudest N on their own

A conductor round trip per speaker change is far too slow, and 50 audio streams to everyone is
wasteful. So audio is not routed per stream by the plan. Instead:

- **The plan provides** an audio overlay over the hubs (a loop-free tree, or two for redundancy),
  the value of N, priority groups (e.g. host or pinned speakers always pass; server-muted never
  pass), and hysteresis parameters.
- **Publishers** stamp each audio frame with a level in the relay-readable header and send to their
  assigned hubs (two by default). Silent or muted clients send nothing (DTX).
- **Each hub** ranks every audio stream it currently sees — from its own clients and from
  neighbouring hubs — and
  - delivers the top N to each of its clients (excluding the client's own), and
  - forwards on each overlay link the top N of the streams it did not learn from that link.
- **Why this converges without coordination:** a stream that is in the meeting-wide top N is in the
  top N at every hub that sees it, so it propagates across the whole overlay; a stream that is not
  stops at the first hub where it loses. No link ever carries more than N audio streams.
- Declared levels are untrusted; priority groups and a per-participant churn limit bound abuse.
- Receivers dedupe when the same stream arrives via two hubs.

**Speaker-following video** uses the same idea one step removed: the plan gives every hub a *route*
for each video stream (which upstream to get it from), and a hub pulls a stream along that route
when one of its clients' slots needs it — because of an explicit pin or because the slot is bound
to "current speaker". So the plan is a routing table plus rules; per-stream demand is local.

### Conductor leave

1. Leaving conductor publishes a final plan naming successor S as conductor with `term + 1`.
   S already holds roster, reports and plan, so this is one flooded message — well under a second.
2. If the leaver is also a hub, it then drains as any hub does (§6), now directed by S.
3. The UI shows "rebalancing meeting, then exiting". A hard cap (a few seconds) bounds the whole
   sequence; past it the node exits anyway and the meeting treats it as a crash.

So the conductor role adds one round trip to an ordinary leave; the node never lingers.

### Conductor crash

- The conductor floods a heartbeat (its report). Successors watch for it.
- On N missed heartbeats, the first live successor in the list claims `term + 1` and floods a plan.
  Nodes accept the highest (term, successor rank). A lower-ranked successor waits proportionally
  longer before claiming, so simultaneous claims are rare and resolve deterministically.
- **No media impact**: forwarding continues on the last plan. Only changes (joins, rebalancing)
  wait, for roughly the detection time (target 1–2 s).
- If the crashed node was also a hub, clients apply contingency routes at once; the new conductor
  then issues a clean plan.
- The successor list prefers stable, well-connected nodes that are not carrying most of the media.

### Partition

A partition yields two connected components, each with (or electing) a conductor and working media
inside itself. On heal, the lower (term, id) side adopts the other's plan. Accepted, stated.

## 5. Topology

### Inputs

The **link graph** from reports: which pairs are connected, which pairs cannot connect, RTT and
loss per link, and per-node capacity. The conductor may ask nodes to attempt new connections
(via introducers) to improve the graph.

### Hub eligibility and score

Eligible: reachable by enough peers, uplink headroom, and not battery-only / metered /
thermally limited unless nothing better exists. Score: reachability, uplink capacity, CPU headroom,
RTT centrality, session age, with a stable tie-break and hysteresis so hubs do not flap.

### Shape

- **Each client connects to two or more hubs where the link graph allows.** The plan spreads its
  sends and receives across them (e.g. audio via two hubs, video slots split by load and latency).
- **The hub graph is connected; a full mesh is preferred but not required.** For each stream the
  plan contains an explicit, loop-free distribution tree over hubs, built to minimise hops and
  latency. Typical path: publisher → hub → hub → subscriber; longer where connectivity forces it.
- A hub carries a stream only if a downstream hub or client needs it (last-N and slots keep
  inter-hub traffic proportional to what is watched).
- A publisher may send a stream to several hubs directly instead of relying on hub-to-hub
  forwarding; the conductor trades client uplink against hub uplink.

```
   c1 ══╦══ H1 ◄──────► H2 ══╦══ c4        ══  client↔hub (clients attach to ≥2 hubs)
   c2 ══╬══╗ ▲           ▲ ╔══╬══ c5        ──  hub↔hub
   c3 ══╝  ╚═╪═══ H3 ════╪═╝  ╚══ c6            H1–H2–H3 need not be a full mesh
             └────►◄─────┘
```

### Capacity (estimates; to be measured)

Hub **uplink** is the binding constraint. A subscriber receiving 5 video slots at mixed simulcast
layers plus audio needs very roughly 2–4 Mbps, so a 20 Mbps residential uplink serves about 5–8
subscribers. A 50-person meeting therefore needs on the order of 6–10 hubs on residential links,
fewer on office or fibre links. Simulcast and last-N are required, not optional.

### Mixing

A hub never mixes. Mixing is a **mixer participant**: an SDK client (typically co-located with a
hub) that subscribes to sources and publishes a composite, e.g. mixed audio for a weak mobile
device. Recorders, transcribers and AI participants are the same kind of thing.

## 6. Leave and crash

### Graceful hub leave or resignation (no media impact)

1. Node floods `DRAINING` (leaving, or resigning the hub role: on battery, thermal, user choice).
2. Conductor issues plan G+1 with that hub's edges moved onto other hubs. Because clients already
   hold connections to several hubs, this is usually only a change of directives.
3. **Make**: affected publishers send to old and new hubs (encode once, send to both); affected
   subscribers receive from both. New upstream hubs request keyframes where needed.
4. **Switch**: receivers dedupe by (sender, stream sequence). Audio is continuous. Each video
   output switches source on a switch-point frame, so nothing undecodable is delivered.
5. Each node's report shows "applied G+1".
6. **Break**: once all dependants have applied G+1 (or the hard cap of a few seconds expires), the
   conductor issues G+2 removing the node, which then closes connections.

Conductor-initiated rebalancing uses the same steps.

### Hub crash (minimal impact)

1. **Detect**: audio flows every 20 ms, so silence on all streams from a hub plus a failed QUIC
   probe is detectable in roughly 200–500 ms.
2. **React locally**: every plan carries **contingency routes** — for each hub, what its neighbours
   do if it disappears (which hub each affected stream and slot moves to). Nodes apply these on
   detection without waiting for the conductor.
3. **Reconcile**: the conductor sees the reports and issues a clean plan.

Impact depends on what the plan was already doing:

- A stream already flowing via two hubs (the default for **audio**: roughly one extra audio stream
  of uplink per client) has **no gap**; the receiver was already deduping.
- A stream flowing via one hub (typically video) freezes until the contingency route delivers a
  keyframe: detection time plus one keyframe round trip.

### Other cases

- Non-hub client leaves or crashes: roster and plan update only.
- Conductor leave or crash: §4.
- Node unreachable from part of the meeting: it is a link-graph change; the plan routes around it.

## 7. Media transport

**Direction (see [04-moq.md](04-moq.md)):** our own conductor-driven protocol with a
**MoQ-compatible data plane** — streams identified by compact numeric ids (sender id, stream
number), *not* MoQ's string track names; publisher-assigned group and object ids, dedupe by
(sender, stream, group, object), MoQ-style keyframe request and drain signals, immutable vs
relay-mutable metadata. MoQ names appear only at the edge where we talk to a public MoQ relay
network, as a mechanical mapping. MoQ itself leaves relay-to-relay routing, slots/last-N and crash recovery out of scope,
and the main Rust implementation routes automatically rather than by explicit policy. A phase 0
spike decides whether `moq-net` can serve as the per-link library underneath. One open point from
that work: whether a slot is filled by rewriting a hop-local id (as below) or by the hub delivering
a different named track, which sits better with frame authentication.

- **Audio in QUIC datagrams, one frame each; video one group of pictures per unidirectional
  stream.** Resetting a stream abandons that group.
- **Header split into a publisher region (immutable end to end) and a relay region (hop-local,
  rewritten by each hub).** End-to-end stream sequence for dedupe and ordering; hop sequence for
  per-link loss.
- **Switch-point flag** as in principle 6; **on-demand keyframes** requested hop by hop and
  deduped by group counter; per-destination abandonment; cut-through forwarding.
- **Slots and capability-based receive**: a client declares what it can decode (slots with upper
  bounds); the plan and hub selection decide who fills them. Slot state is explicit on the wire.
- **Multi-hop**: relay ids are hop-local, so chaining hubs needs nothing extra in the header.
- **Multi-path**: the same stream may arrive via two hubs; receivers dedupe on the publisher region.
- **Congestion**: encoder bitrate and simulcast layer driven by QUIC connection stats and
  hop-sequence loss; hubs drop per destination rather than stall.
- **Transport library**: iroh 1.0 preferred, behind a trait so quinn (or, later, a WebRTC
  data-channel transport for browsers) can sit beside it.

### Security, phased

- **Phase A:** QUIC/TLS per hop between PAKE-authenticated members. All crypto happens on member
  devices. Payload is opaque to hubs from day one so phase B is not a rewrite.
- **Phase B:** per-frame encryption (SFrame construction, encrypt once and fan out) under sender
  keys wrapped by a meeting key that the conductor generates, distributes over authenticated
  control channels and rotates on leave/kick; Ed25519 sender signatures for attribution. Media is
  then opaque to non-member relays, and a kicked member loses access.

### Media pipeline

Use libwebrtc where it is the best tool, via the LiveKit Rust crates:

- **Audio processing (AEC/NS/AGC): libwebrtc APM from the start**, with platform voice-processing
  units on Apple/Android where they are better.
- **Capture and codecs**: spike whether libwebrtc's device modules and codec factories are usable
  outside PeerConnection; otherwise `cpal`/`nokhwa`/`scap`, Opus, platform H.264 with a software
  fallback.
- **Jitter buffer / playout**: use NetEQ if it can be driven standalone; otherwise our own adaptive
  playout with Opus PLC/FEC.
- Not used: libwebrtc's PeerConnection/ICE/SRTP transport.

## 8. SDK and headless clients

The `sdk` crate is the only way anything talks to a meeting.

- Join/leave, roster and role events, mute, slot declarations, stats.
- Media in and out at two levels: **raw** (PCM, video frames) and **encoded** (bring your own
  codec or pass-through).
- Options to run without devices, without a hub role, or as hub-only.
- Rust first; UniFFI bindings (Python is the obvious first for AI/transcription integrations).

`apps/bot` is the reference headless client: synthetic or file media, scripted behaviour
(join at T, mute, leave, crash), metrics and event log on a local endpoint. One binary serves
automated tests, manual dev testing, load generation, the reachable-relay role (§3), and as the
skeleton for recorder/transcriber/AI participants.

## 9. Operability without servers

- **Local first**: structured event log and a metrics ring per node. Every plan is logged with its
  generation and inputs; since every node holds the plan, one node's log explains the meeting.
- **In-call stats overlay**: per slot — source, hub path, bitrate, loss by hop, jitter, RTT.
- **Meeting health view**: reports are already flooded to hubs and successors; expose "who is
  hubbing, who is struggling, why" to participants.
- **Diagnostics bundle**: one click exports logs, metrics and plan history, redacted (no media, no
  keys, addresses hashed unless opted in).
- **Optional telemetry sink**: off by default; an org can configure an OTLP endpoint.
- **Version skew**: protocol version negotiated at admission; the conductor picks a meeting-wide
  version from declared capabilities; too-old clients are refused with a clear message.
- No participant or meeting identifiers as metric labels.

## 10. Testing strategy

| Layer | What | How |
|---|---|---|
| Codec | Frame header encode/decode | Test vectors + fuzzing |
| `hub-core`, `topology` | Pure functions | Unit + property tests ("plan is loop-free", "no hub over budget", "every client served given a connected graph") |
| Control + topology | Join, leave, crash, partition, conductor loss, **non-full-mesh graphs** | Deterministic simulator: virtual clock, scripted network (latency, loss, which pairs can connect), seeded and replayable |
| Simulation invariants | "Graceful leave loses zero frames"; "crash recovery ≤ X ms"; "one conductor per connected component"; "conductor leave completes within the cap" | Thousands of seeds in CI |
| **N headless bots, local** | Real QUIC, real engine, synthetic media | `bot` processes on one machine, optionally in network namespaces with netem (NAT classes, loss, jitter, blocked pairs) |
| **N headless bots, distributed** | Real networks | Same `bot` binary on several machines, driven by a scenario runner (start/stop/script over SSH or a small control socket), metrics pulled back for assertions. Used for automated runs and manual dev sessions ("join my meeting with 20 bots") |
| Media quality | Audio continuity across migration; A/V sync; AEC double-talk | Synthetic sources with embedded marks; native runs on Windows/macOS |
| Soak / chaos | Long meetings with random leave/crash/throttle | Bots, nightly |
| Platforms | Build + smoke | CI matrix: Linux, Windows, macOS from the start |

## 11. Browsers

A browser page cannot be a hub in this design for technical, not performance, reasons:

- It cannot listen for inbound connections or open UDP sockets, so it cannot accept QUIC or do DHT
  or mDNS discovery. WebTransport is client-only.
- The only browser-to-browser path is WebRTC, which needs a signalling channel for every
  connection and is a different transport stack from QUIC.

A browser hub is *possible* by adding a WebRTC data-channel transport and letting members relay
signalling (the SnoW paper did roughly this), but it means a second transport, a web-reachable
rendezvous, and tab-throttling problems. Recommendation: browsers, if ever needed, are **clients of
native hubs** — over WebTransport where the hub is directly reachable, or over a WebRTC data
channel (hub side via `str0m`) where it needs hole punching. The transport trait keeps this open.

## 12. Risks

1. **First-contact NAT traversal** without an operated relay. Spike first; TURN-like fallback is
   accepted.
2. **Uplink limits** on residential hubs at 25–50 participants; depends on simulcast and good hub
   selection.
3. **Plan computation quality** on awkward link graphs (few reachable nodes, asymmetric links).
4. **Flooded control state**: correctness of dedupe/repair, and convergence time at 50 nodes.
5. **libwebrtc à la carte**: how much is usable outside PeerConnection is unverified.
6. **Dependency immaturity**: iroh-live preview, MoQ churn, hardware codec crates uneven.
7. **Low-entropy passwords** vs offline guessing on DHT records; default to generated links.
8. **Patents** (Topia mixed mesh/forwarding; Skype-era supernode).

## 13. Decisions so far

| Topic | Decision |
|---|---|
| Name | Palaver |
| Licence | Open source, permissive; avoid GPL and static-LGPL dependencies |
| Scale | 25–50 participants, single conductor; multi-hub from the start |
| Infrastructure | Public STUN-like services fine; TURN-like relay acceptable as last resort |
| Identity | Anonymous user-provided names; SSO/org directory later |
| Roles | Simple host: meeting creator / first joiner |
| dark_tower | Separate project; copy code as needed, share nothing |
| UI shell | Deferred; must be clean, professional, strong media support |
| libwebrtc | Use where it makes sense |
| Integrations | SDK for headless clients, recording, transcription, AI participants |
| Control dissemination | Flooding with digest repair; no gossip library at this scale |
| Audio | Sent via two hubs by default; hubs forward loudest N autonomously |
| Leave | Hard cap 3 s |
| Host succession | Longest-present member (co-host role later) |
| Stream identity | Numeric ids internally; MoQ track names only at the relay-network edge |

## 14. Open questions

- **Public MoQ relay network**: roadmap in [04-moq.md](04-moq.md) §Roadmap — which provider(s),
  which draft version, and whether it carries audio acceptably without datagrams.
- **`moq-net` per link** vs fully own protocol — phase 0 spike.
- **Slot filling**: hop-local id rewrite, or the hub switching which stream it delivers into a
  slot (needed if frame authentication covers stream identity).
