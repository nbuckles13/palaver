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
| `node` | One node's core: the khef and the control logic driven together, and the rules that join them (carry out the plan, report links, find the meeting when alone) | none |
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

- **Ticket**: the meeting secret, the iroh-relay URL, and one or more entry points, each a
  public key with its addresses. It is a snapshot built from the current roster whenever someone
  copies an invite (the copying node plus one or two others), not something that updates after
  it is shared. It keeps working while any entry point is reachable at a listed address or
  through the iroh-relay by key.
  As text it is `palaver:` followed by base32 (case does not matter): a version byte, the
  fields in the same numbered encoding as reports, and a checksum, so a ticket cut short by
  a chat program is refused rather than half used. At most four entry points with eight
  addresses each; a typical ticket is under 300 characters. The text contains the meeting
  secret, so the code hands it out wrapped as a secret and never logs it.
- **Addresses**: a node is identified by its key; its addresses are hints. It advertises a
  capped list (about eight): private and public IPv4 and routable IPv6, one per usable
  interface; not loopback (a test option may add it, for machines with no network) and not
  IPv6 link-local. A peer tries all of them and uses whichever answers. A node on two networks
  therefore bridges them with no special handling. Every member learns every other member's
  addresses; in code they are redacted like names. Addresses a node *observes* for its
  neighbours (the introducer function) are added with NAT traversal.
- **Derivations**: SHA-256 and HKDF. The meeting tag is an HKDF output of the meeting secret;
  a participant id is the first 16 bytes of the SHA-256 of the node's public key.
- **Rendezvous** is pluggable and merged: invite ticket (as above), mDNS/LAN,
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

`meeting` is an opaque **meeting tag** and `participant` a short node id; nothing human-readable
is on the wire.

- **Meeting tag**: derived from the meeting secret (a key-derivation output, truncated), so every
  member computes it without negotiation, it reveals nothing to a relay outside the meeting,
  and two meetings cannot collide. It is kept in every name even though the khef network is
  per-meeting today, because a reachable headless khef or a public relay may carry several
  meetings. A breakout gets its own tag, derived from the same secret plus a breakout id.
- **Participant id**: the first 16 bytes of a hash of the node's per-meeting public key. It is
  self-certifying: reports are signed, so anyone can check that an id belongs to the key that
  signed, and no member can claim another's id. A device that rejoins with the same key keeps
  its id; a new key is a new participant. After subscription a track is a numeric alias. A standard
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
- **Encoding**: numbered fields as MoQ key-value pairs (even numbers carry integers, odd ones
  bytes; a nested structure is a bytes field; a repeated field repeats its number). A reader
  skips numbers it does not know, so nodes on different app versions can share a meeting. A
  number is never reused.
- **Signing**: the object is `payload | signature`, signed as sent with the node's per-meeting
  key, so no canonical encoding is needed. The signed bytes also carry a label for the kind of
  object, the meeting tag and the sequence number, which stops replay as another kind, into
  another meeting, or as a newer object. A report carries its public key, and the receiver
  checks the key hashes to the participant id in the track name. A plan is signed by its
  conductor, whose key is in the roster. `proto` does no cryptography: signing and verifying
  come through traits.
- **Cheap checks first**: a plan floods in over every link, so most arrivals are copies. A
  node settles a plan whose group id is not higher than the one it holds without checking
  the signature, as it already does for reports. The conductor looks at its plan again only
  when a report differs in something the plan depends on (identity, links and their path
  kind, unreachable nodes); round-trip times alone do not trigger it.
- **Who may issue a plan**: a node adopts a plan of the same term only from the same conductor
  with a later generation, and a plan of the next term only if its conductor is on the held
  plan's successor list. A joiner has nothing to check against and adopts the first verified
  plan it receives.
- **On tracks**: each report is one object in a new group, the group id being its sequence
  number. A plan's group id is its term (high half) and generation, so ids keep rising across a
  change of conductor. A khef keeps the latest group of every control track, so a joiner
  subscribing from the current group gets the plan and every report without `FETCH`.
- **Flood rule in detail** (`khef_core::Flood`): an object is passed on only if its group id
  is higher than the last seen for its track, to every neighbour except the one it came from.
  A khef checks the signature before accepting an object, so a member cannot shut out
  another's reports with a forged high group id. A report is dropped from the cache 100 s
  after it arrives, three missed refreshes (its group id is remembered, so it cannot come back from a neighbour that
  still holds it); the plan is kept until replaced. The number of tracks remembered is capped.
- **Liveness: no flooded heartbeat.** Whether a node is still there is judged link by link,
  by its neighbours, and reaches the conductor as a change in their reports.
  - *Watching a link* (`khef_core::LinkWatch`): anything a khef sends shows its forwarding
    loop is running. When a khef has sent nothing on a link for 200 ms it sends a filler (a
    MoQ padding datagram, which the receiver discards). A link silent for 700 ms is dead.
    The transport's own keep-alive is not used: the transport's task can keep answering
    while the khef is stuck.
  - *Reporting it*: a link that comes up or dies changes the node's report, which is sent
    at once.
  - *The roster*: a link is up while both its ends list it. A node is in the roster while a
    chain of such links joins it to the conductor. So a dead node goes as soon as its
    neighbours say its links are gone, and anything reachable only through it goes too.
  - *Hold*: the conductor keeps a node for 500 ms after it stops being reachable, and
    ignores neighbours' "cannot reach it" in that time, so a link that flaps or a node
    moving between links costs no plan, and a crash costs one.
  - *Backstop*: every node repeats its report every 30 s. A report not refreshed for 100 s
    is not believed, which catches a node whose links are alive but whose control logic has
    stopped. The khef and the control logic are to be driven from one loop, so that this is
    rare.
  - Measured in the simulator, steady state, per node (reports as payload bytes; a filler
    counted as 40 bytes on the wire):

    | Nodes | Before: kbit/s | Now: reports kbit/s | Now: fillers/s | Now: fillers kbit/s |
    |---|---|---|---|---|
    | 5 | 15 | 0.5 | 14 | 4 |
    | 10 | 39 | 1.3 | 17 | 5 |
    | 25 | 115 | 3.8 | 18 | 6 |
    | 50 | 243 | 8.1 | 18 | 6 |
    | 100 | 498 | 17 | 16 | 5 |

    Fillers go only to neighbours and do not grow with the meeting; they stop on a link
    that is carrying audio. A crashed node is out of the plan in about 1.2 s (was 3.5 s),
    with one plan.
- **Cadence**: a node sends a report at once when something in it changes, and every 30 s
  regardless, with two limits that keep a plan from setting off a storm of reports (each
  report floods to every node):
  - *Only the nodes a plan asks something new of report it at once*: those whose own entries
    (peerings, audio links; later routes and budgets) differ from the previous plan. The
    conductor computes the same set from the same two plans and waits only for those. For
    everyone else "last plan applied" rides on the next regular report, so it is not a
    meeting-wide "everyone has this plan" signal.
  - *Never two reports within 200 ms*: changes inside that gap share one report.
  - Measured in the simulator, control objects sent while 50 nodes join (taken when reports
    were still repeated every second, which alone was about 7,250 objects a second):

    | Joins | Neither limit | Gap only | Affected only | Both |
    |---|---|---|---|---|
    | 40 ms apart, over 4 s | 167,000 | 50,000 | 65,000 | 36,000 |
    | 200 ms apart, over 12 s | 174,000 | 144,000 | 84,000 | 62,000 |
    | 1 s apart, over 51 s | 203,000 | 188,000 | 165,000 | 154,000 |
    | All in the same instant, over 3 s | 65,000 | 36,000 | 53,000 | 36,000 |

    The gap handles bursts; the affected-only rule handles joins spaced wider than the gap,
    where the gap does nothing. The gap delays tidying up (closing links the plan no longer
    names) from about 80 ms to about 220 ms after a join; audio wiring is not delayed.
  - Phase 1 sends whole plans, not deltas.
- **Phase 1 fields**. Report: public key, name, sequence number, links held (peer, path kind,
  RTT), unreachable peers, last plan applied, up to eight addresses, iroh-relay URL. Plan: term and generation, conductor, roster,
  peerings, audio overlay tree and N, successor list, each member's tracks with their
  aliases, and the next free aliases. Report also: the media tracks the node can send. The rest of the lists above arrives as
  new field numbers.
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

### Startup, step by step (phase 1, one network)

- **Create**: the creator derives the tag and its id, starts endpoint, khef and client, and is
  conductor. Its first plan is term 1 with a roster of one. The conductor logic is an ordinary
  client of its own khef: it reads reports and publishes the plan through subscriptions like
  any other node.
- **Connect**: khef-to-khef connections use a Palaver protocol identifier with a version,
  distinct from the standard MoQ one, which is kept for a future standard client edge.
- **Admission in phase 1**: each side proves knowledge of the meeting secret, bound to the
  connection, before any MoQ message. The password handshake replaces this in phase 4.
- **Control flood**: on connect each khef sends `SUBSCRIBE_TRACKS` for the control prefix. The
  other side answers with a `PUBLISH` per control track and sends the latest group it holds of
  each, then new ones as they arrive. Control objects travel on streams, not datagrams (a plan
  is several kilobytes). An object is never sent back to the neighbour it came from.
- **Roster**: the conductor adds any admitted node whose report verifies once a chain of
  links, each reported by both its ends, joins the node to it; and drops a node 500 ms
  after that stops being so (see Liveness above). Host approval and meeting lock come later.
- **Peerings**: on one network the node with the lower participant id dials. With NAT
  traversal both dial at once and one connection is kept by the same tie-break.
- **First report**: a joiner's control logic starts when its entry link is up, so its first
  report names that link and the conductor wires it by a link that already works.
- **Closing unplanned links**: a node closes a link the plan does not name only when both
  ends are in the roster and both are wired as the plan says. It knows how the other end
  stands from that node's report (plan applied, links held). Until then the link may be the
  other end's only way to receive the plan, or to be heard of at all.
- **Cut off**: a node left with no link at all dials the members of the last plan it saw,
  one after another, until one answers, and carries on as a joiner would. The others cannot
  do this for it: they cannot tell a node that was cut off from one that left.
- These three rules were found by running the real control logic in the simulator: without
  them the conductor hangs up on every joiner, a khef hangs up on a joiner whose planned
  links are still being dialled, and a node whose neighbours all crash never returns.
- **Audio wiring**: for each overlay link in the plan, each khef sends `SUBSCRIBE_TRACKS` for
  the audio prefix; every audio track is announced with `PUBLISH`; top-N gates only the
  datagrams. N comes from the plan, which every node holds.

### Conductor leave, crash, partition

- **Leave**: publish a final plan naming the successor with `term + 1` (the successor already
  holds full state), then drain as a transit khef if it is one. UI: "rebalancing meeting, then
  exiting". Hard cap **3 s**, after which the node exits and is treated as a crash.
- **Crash**: successors learn the conductor is gone the way anyone's loss is learnt, from the
  reports of its neighbours; the first live successor claims
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
- **Phase 1 rule** (`topology::wire`; scoring and capacity come later):
  - *Change as little as possible*: every edge of the plan in force stays while both ends are
    in the roster and neither reports the other unreachable. A join disturbs nobody else.
  - *Audio tree*: grown from the conductor one edge at a time. The edge chosen is the best by,
    in order: a link already up and direct, then a pair not tried yet, then a link through an
    iroh-relay; the attachment point nearest the conductor; lowest round-trip time; lowest
    ids. At most 4 tree links per node unless a node can be reached no other way, which bounds
    a node's audio uplink to 4 × N tracks.
  - *Peerings*: every tree edge, plus, for each node, its nearest nodes until it has two, so
    the control flood has a second path and a replacement tree edge is already connected. A
    node that already has six is passed over while another will do; otherwise the node
    everyone joined through would be everyone's second peering.
  - The conductor re-runs the rule when a report or its own links change or a member times
    out, and issues a plan only if the roster or wiring differs from the one in force (a
    round-trip time that moves does not cause a plan).
  - The phase 1 rule never moves an edge that still works, and a member the tree cannot
    reach stays in the roster without audio; both change with the scored planner.
- **Rebalancing** (phase 3, with the scored planner). The conductor reshapes a working
  meeting, not only a broken one: to cut latency (a shorter or lower-RTT path exists) and to
  move load off a congested or lossy link or an overloaded khef. Rules:
  - *Always hitless*: every move is make-before-break (section 9). The new path is brought
    up and delivering before the old one is dropped; khefs dedupe in between. For video
    routes that is a per-track switch; for the audio overlay the new tree runs alongside the
    old and the old is retired once every node reports the new one applied, which is the same
    machinery as the two standing trees.
  - *Hysteresis*: a move must be worth a clear margin and a node or track is not moved again
    for a hold time, so plans do not flap on a round-trip time that wanders.
  - *Bounded*: a plan moves a few edges at a time, so a mistake is small and the next
    report shows whether it helped.
  - Tested in the simulator with the invariant "zero lost objects across any plan change".
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
   *Deferred optimisation*: serve a joiner from frames already sent instead of asking for a
   keyframe, which costs every subscriber of the track a large frame and bunches up when many
   people join at once. A khef would keep the current group of each video track it carries and
   a subscriber would ask to start at the beginning of that group (a location filter, already
   in the codec; `FETCH` only for anything older). Costs: one group per track in memory at each
   khef, and a joiner decoding up to a group of old frames to reach the live edge.

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
- **Between khefs** the plan pre-wires the audio overlay: loop-free trees that each reach every
  node, as standing top-N subscriptions. Top-N runs only on overlay links, not on every link a
  khef holds.
- **Two trees, with no node in the middle of both.** Losing any one node, the conductor's
  included, cuts at most one tree, so audio continues with no gap and no re-plan. Audio is sent
  on both and deduplicated at each khef; it is small enough that twice costs little. Phase 1
  runs one tree; the second arrives in phase 3 with crash handling. Until then, losing a node
  in the middle of the tree stops audio between the parts it joined until a new plan is issued,
  and if that node was the conductor no plan comes (takeover is also phase 3).
- **Audio timing**: one object per group, the group id being the frame counter, advancing
  through silence. The receiver gets timing from it with no extra field.
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
     the quietest selected track that is not protected (ties: the oldest). A selected track
     is protected while its time is within the **hold** and it is still arriving; one that
     has missed two packets is not. If every selected track is protected, it is not added.
  - **The hold is for a speaker who is still talking**, to stop a slot flapping between
    voices of similar loudness. It does not protect a track that has stopped arriving:
    either its speaker stopped, and the next voice should not wait; or the khef before this
    one dropped it for another, and holding its slot would add this khef's hold to that
    one's at every hop. Measured with four talkers starting together and N = 3: the third
    loudest is 300 ms late for every listener, however many khefs away (250 ms of hold where
    the talkers meet, then 40 ms once for the dropped track to be seen to have stopped).
    Before this rule it was 260 ms more at every khef.
  - **Fast in, slow out**: entry happens on the first speech packet; a track leaves only when
    someone else needs the slot and it has stopped, or has not ranked in for the hold time. Slots are never
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
  `SETUP` (unknown setup options are ignored by other implementations). Our behaviour beyond
  the draft (top-N selection on the audio prefix, the control flood) applies only on
  connections where both sides sent it; a peer that did not is treated as a plain draft 22
  endpoint. The option is type `0x50414C57` (odd, so it carries bytes; not a grease value)
  holding a version. The N of top-N is not here: it comes from the plan, which every node
  holds. Fields added by later versions
  are ignored by older ones, and the lower of the two versions is the one in force. The number
  is self-assigned: the draft's setup option registry has no private-use range and asks for a
  provisional registration, which we should request before interoperating with others.
- **Transport**: iroh; always QUIC end to end, over a direct UDP path or an iroh-relay path.
  Introduction is done by members over the control tracks, with iroh-relay as the helper and
  fallback (spike 1); quinn as fallback; behind a trait that offers request streams, unidirectional
  streams and datagrams.
- **The node** (`palaver_node::Node`) owns the khef and the control logic, which do not know
  of each other. Control objects go up from the khef to be checked and come down to be
  flooded; an adopted plan is carried out (dial planned peers, tell the khef where audio
  goes, close unplanned links once both ends are wired); link changes go into the node's
  report. It is sans-IO, so the simulator runs the same code the product will: every
  simulated node is a real `Node`, and what crosses the simulated links is real MoQ.
- **The khef** (`khef_core::Khef`) holds a session and a link watch per neighbour and ties
  them to the flood and to top-N:
  - *Control*: an object that arrives is handed up to the node's control logic, which
    checks it; only what is handed back is kept and flooded. A neighbour that asks for the
    control prefix is given the latest of every control track.
  - *Audio*: on each link of the plan's tree both ends ask for the audio prefix and
    announce every audio track in the plan, under the plan's aliases. A packet from a tree
    link or from the node's own microphone is offered to the selector of every other tree
    link and of the node's client; the khef answers with the list of links to send the
    caller's buffer to, unchanged. Audio is not taken from a link that is not in the tree,
    nor for an alias the plan does not list.
  - *Links*: silent for 700 ms is reported dead and dropped; idle for 200 ms gets a filler.
- **Objects in the session** (`khef_core::Session`):
  - *Datagrams* (audio) are decoded and encoded on the caller's buffers; the session keeps
    nothing, and the payload handed back is a slice of the datagram that came in, so it
    can go on to the jitter buffer or to another link without a copy. A datagram for an
    alias not announced is dropped.
  - *Whole objects on their own stream* (control tracks, up to 256 KB) are copied once each
    way. An object stream that overtakes the PUBLISH naming its alias is held for it (at
    most 16 such streams). An object too large, or a stream cut short, costs the stream,
    not the session.
  - Video needs the streaming form (payload passed on as it arrives); it comes with video.
- **One alias per track, meeting-wide.** The draft has each publisher pick an alias per
  connection, so a khef forwarding a datagram would rewrite its header and copy it once per
  outgoing link. Instead every khef uses the same alias for a track, so a datagram is
  forwarded byte for byte: no rewrite, one shared buffer for all links, no per-link alias
  map on the forwarding path. The session takes the alias from the khef and refuses one
  already in use on that connection.
- **Where the alias comes from: the conductor assigns it.**
  - *Offer*: a node's report lists every media track it can send, including those it is
    not sending now (all simulcast renditions, a screen share not yet started); at most 32.
  - *Assign*: the plan gives each offered track an alias, listed under its member. A
    track keeps its alias for as long as it is offered. A new track takes the next free
    one; an alias is never given twice in a meeting (the plan carries the next free numbers,
    so a later conductor carries on from them), so a late packet of a track that has gone
    cannot be taken for another's.
  - *Size is the conductor's policy*, since the alias is in every packet: a number up to
    127 takes one byte, up to 16,383 two. Audio tracks, whose packets are about 80 bytes,
    are given the small ones (one byte for the first 128 audio tracks of a meeting); every
    other track takes numbers from 16,384 up (three bytes), which is nothing to a video
    packet. The policy can change without khefs changing.
  - *Khefs do not choose or check*: they look the alias up in the plan when they announce
    a track, and forward. A track that is not in the plan has no alias and cannot be sent,
    so this is also where a cap on what a member may publish will live.
  - *No fixed list of renditions*: a track is whatever a node offers, by name.
  - *Report and plan tracks* must flow before a node is in any plan, so they do not wait
    for the conductor: a report track's alias is taken from the participant id (already a
    hash of the node's key; contriving a clash takes about 2^60 tries), the plan track's
    is fixed, both above anything a plan may assign.
  - Cost: about 10 bytes in the plan per track, so 3 to 4 KB more for a 50-person meeting
    with eight tracks each while plans are sent whole. Adding a track mid-meeting takes a
    round trip through the conductor; offering everything up front avoids that.
  - Considered and dropped: a hash of the track's name (wide enough to be safe, it costs 8
    bytes in every audio packet), and a formula over a per-member number (every node must
    then agree on the formula and on a fixed list of renditions).
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
  - It decides, once per frame interval, what the decoder plays: the frame; the frame
    rebuilt from the next packet's FEC or a later packet's DRED; concealment; silence; or a
    frame of filler.
  - *Target delay*: enough to cover the 95th percentile of how late packets arrived over
    the last 3 s, plus a frame; between 40 and 500 ms; up at once, down a frame every 2 s.
  - *Steering*: the delay in force is measured directly (how long after its earliest
    possible arrival the frame at the playhead is played). A frame or more above target for
    a second: skip a frame, a quiet one by `audio_level` if possible. A frame or more below
    for 100 ms: play a frame of filler.
  - *Running dry*: the decoder conceals, then outputs silence after 100 ms. A packet for a
    frame concealed in that spell is still played (playout steps back), so a stall on the
    network costs a gap but no audio.
  - Timing comes from the frame counter alone, which is why it advances through silence.
  - It never looks inside a packet and is generic over what holds the bytes, so a
    reference-counted slice of the received datagram passes from the socket to the decoder
    with no copy and no allocation per packet.
  - First version: skipping and filler are whole frames; time-stretching inside a frame
    comes with the decoder. Numbers are to be tuned by ear.
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

Working out afterwards why media did not get somewhere is not designed yet; see "Post-mortem
debugging of packet flows" in the backlog of the plan.

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
| Audio | Top-N selected by khefs over a pre-wired overlay of two loop-free trees sharing no middle node (one tree in phase 1); loudness-ranked, fast in, slow out; all audio tracks announced at join; DTX, FEC and DRED |
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
