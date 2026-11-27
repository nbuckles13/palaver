# Palaver — design

Status: draft 3, 2026-11-15. The MoQ-based design, confirmed. It merges the two earlier drafts
kept in `archive/` (the original slot-based design and the MoQ-native alternative).

Terms: a **node** is one running instance (app or headless). A node contains a **client**
(capture, encode, decode, render) and a **khef** (a small MoQ relay of our own). Every khef is an
**edge khef** for its own client; the conductor also uses some as **transit khefs** ("hubs").
One node at a time holds the **conductor** role. There is no relay server: "relay" alone
is not used for a khef. MoQT = Media over QUIC Transport; facts about it
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

- **Ticket**: the meeting secret and one or more entry points, each a
  public key with its addresses. It is a snapshot built from the current roster whenever someone
  copies an invite (the copying node plus one or two others), not something that updates after
  it is shared. It keeps working while any entry point is reachable at a listed address.
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
  The tag is 16 bytes of HKDF-SHA-256 with no salt and the label `palaver meeting tag v1`.
  Signatures are Ed25519, verified strictly, so that every node agrees on what verifies.
  These live in `net` (`identity`); the sans-IO crates take them through traits.
- **Rendezvous** is pluggable and merged: invite ticket (as above), mDNS/LAN,
  mainline DHT under `K_topic`, optionally Nostr relays or a public MoQ relay network.
- **Reachability** (decided 2026-11-20: quinn, not iroh; see section 12). No relay server
  is part of the product. What gets two nodes connected, in the order it is being built:
  1. *Direct*: a node on a LAN, with a public address, or behind a router with a port
     mapped is dialled at its address.
  2. *Punching inside a meeting*: each node reports the public address it observes for
     each neighbour. When the plan pairs two nodes, both dial each other at once, so each
     router sees an outgoing packet before the other's arrive.
  3. *STUN from the node's own socket*: a node learns its public address before it knows
     anyone, for the ticket and its report; and by asking two servers it can tell that its
     router gives a different port per destination (symmetric NAT), where punching fails.
     The question and its answer use the QUIC socket, since the address a router gives is
     per socket; a layer under QUIC takes the answers out (`net::stun`). The server learns
     the node's address and that it asked, and nothing of the meeting. A node asks when
     it makes an invitation, since that is when its public address is wanted, and by
     default only if it has no public address of its own (`net::reach`). Whether to ask,
     and whom, are the user's to set. Not possible at that moment: sparing the question
     when the person invited is on the same network, because that is not yet known. An
     invitation marked as for someone nearby could skip it. What comes back is one of:
     open, behind a router (the address goes into the ticket and the report), a port
     per destination (no address to give, and the user can be told a punch will not
     work), or unknown.
  4. *The ticket comes back*: when nobody in a meeting is reachable from outside, a joiner
     adds itself to the ticket and sends it back the way it came; a member pastes it and
     dials. Needed once per joiner (to reach one member; the rest follow by step 2).
     Members on one network (same public address) need only one entry between them.
     How it runs: a joiner that nobody answers keeps trying everyone in its ticket, a
     second apart, which also keeps the way back through its own router open. It asks
     STUN for its public address and offers a ticket with itself first in it. A member
     given that ticket dials the joiner; since the joiner has been sending to that
     member's address, both routers have now seen a packet go out, and the connection
     is made. The member must be one the joiner's ticket names (in practice, whoever
     invited it): the joiner's router lets in only those the joiner has sent to.
  5. *Rendezvous* (phase 4) does step 4 without the pasting.
  - **Not covered**: a node behind a symmetric NAT can connect only to a node that is
    reachable without punching, and then talks to the others through it. If there is no
    such node, it cannot join. A headless Palaver node on a reachable machine is the
    answer we offer; a public TURN service behind a build feature is
    the alternative if that proves too little.
- **Private mode**: one switch that puts every setting that tells an outsider something
  at its most private value. The list so far: never ask a STUN server. Later entries:
  rendezvous announcements, always-on tickets.
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
  headroom, uplink estimate, metered flag, NAT class, candidate addresses, links held with
  RTT / loss / utilisation, peers it failed to reach, tracks it carries, last plan generation *applied*.
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
- **Phase 1 fields**. Report: public key, name, sequence number, links held (peer,
  RTT, the address the peer is seen at), unreachable peers, last plan applied, up to eight addresses. Plan: term and generation, conductor, roster,
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
  It runs on the first two-way stream (`net::admission`): the dialler sends a version and a
  nonce; the accepter answers with its nonce and its proof; the dialler sends its proof;
  the accepter says "admitted". A proof is an HMAC-SHA-256, under a key derived from the
  meeting secret (HKDF, label `palaver admission key v1`), of which end is proving, both
  transport keys and both nonces, so it is good for one direction of one connection
  between one pair of keys. A wrong proof, an unknown version, or five seconds of nothing
  closes the connection. An endpoint wrapped in `Admitted` gives out only connections
  that passed, so nothing above it sees one that did not. This is sound only because the
  secret is 32 random bytes; a password would be open to guessing by anyone who can dial.
- **Control flood**: on connect each khef sends `SUBSCRIBE_TRACKS` for the control prefix. The
  other side answers with a `PUBLISH` per control track and sends the latest group it holds of
  each, then new ones as they arrive. Control objects travel on streams, not datagrams (a plan
  is several kilobytes). An object is never sent back to the neighbour it came from.
- **Roster**: the conductor adds any admitted node whose report verifies once a chain of
  links, each reported by both its ends, joins the node to it; and drops a node 500 ms
  after that stops being so (see Liveness above). Host approval and meeting lock come later.
- **Peerings**: the node with the lower participant id dials. The other waits 200 ms and,
  if no connection has come, dials too: through NATs the first dial's packets arrive only
  once the other end has sent some, and on an open network the wait saves a second
  connection. If both get through, one is kept by the same tie-break. The first dial alone
  decides whether the peer is reported unreachable.
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
  transit khef where both hops are direct. To be refined (latency, avoiding
  hot spots on transit khefs). Control tracks run over any link.
- **Shape**: every edge khef peers with **two or more** khefs where the graph allows. The
  transit graph is connected; a full mesh is preferred, not required. Two nodes may peer directly.
- **Phase 1 rule** (`topology::wire`; scoring and capacity come later):
  - *Change as little as possible*: every edge of the plan in force stays while both ends are
    in the roster and neither reports the other unreachable. A join disturbs nobody else.
  - *Audio tree*: grown from the conductor one edge at a time. The edge chosen is the best by,
    in order: a link already up, then a pair not tried yet;
    the attachment point nearest the conductor; lowest round-trip time; lowest
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

### When a link cannot carry its video

Not built; the shape agreed 2026-11-24, to be built in the order given at the end.

A link between two khefs carries pictures for the neighbour itself and for everyone
behind it, so what one khef does to a link is felt by people it knows nothing about.
And the reflex of a video system, asking for a key frame, is the wrong one here: a key
frame is the largest thing there is to send, and a link that is behind is the last
place to send one. So the rule is to **shed first and resume when the link has
drained**, and never to mend a full link by asking for more.

**What is measured.** For each link, how long the oldest bytes of each group have
waited to be sent (a time, not a count of bytes: it means the same on a fast link and
a slow one). `MoQ` has the parameter for the limit (a subgroup's delivery timeout).

**Three who can act, each on what only it knows.**

- **The khef at the link, at once** (within a few hundred milliseconds, by itself, and
  only ever sending less):
  1. Drop the frames that nothing depends on, if the track has them. With pictures
     coded in two layers of time, the second layer goes in a subgroup of its own, which
     is a stream of its own: the khef drops it without looking inside, and the picture
     runs at half its rate. (`MoQ` and our wire code already have subgroups; the
     software encoder can make the layers; hardware encoders vary.)
     To whoever is watching, this must not look like loss on the network: the decoder
     is given a whole stream at a lower rate and conceals nothing, and nobody asks for
     a key frame on account of it. It is counted and shown for what it is, a lower
     rate of pictures (in the details, and on the tile itself if it is bad enough).
  2. Abandon the group on that link, and send nothing more of the track there until
     the link has drained. Then pick up at the next group; if none is coming, ask for
     one, no more often than a set interval.
  3. If it keeps happening, stop the track on that link and say so downstream, so that
     the tile shows why: "paused, the network cannot carry it", which is not the same
     as a camera turned off.

  Among several tracks on a link, the least wanted goes first. Each subscription
  carries how much it is wanted (a thumbnail less than whoever is talking, a shared
  screen most), and a khef passes on the most wanted of those behind it.
- **The subscriber, in seconds**: told that its track is being shed on the way, it
  asks for a smaller size in its place. Sizes are separate tracks, so this is an
  ordinary change of subscription, and it is the subscriber that knows how large the
  tile is.
- **The conductor, more slowly and for longer**: each khef's report says where it has
  had to shed and how much. The conductor routes around the link, or sets what the
  link may carry, or caps the size that crosses it. This is the same work as routing
  video apart from audio (plan, backlog).

**Going back up.** Everything above only sends less, and each step needs its way
back. A subscriber that took a smaller size returns to the larger when the conductor
gives it another route, or when the link recovers; a khef that dropped a layer or held
a track resumes it. Knowing that a link could carry more than is being sent on it is
its own subject: probing, and estimating what a link can carry above what it carries
now. None of that is designed yet. It comes with the work on bad networks, after
video runs well on good ones, and with test suites that hold each of these rules to
what is written here.

**Key frames.** Asked for when someone begins to watch or has lost their place, the
requests for the same group joined into one at each khef (below), and the encoder
free to hold them to so many a second. And one by itself at a long interval as a backstop, so that nothing
that goes wrong leaves a picture frozen for longer than that.

**In what order.** First video that runs well on a good network, with none of this
(decided 2026-11-25). Then, with test suites for each: the limit on waiting, abandoning
the group, resuming when drained (2 above, without the layers), and counting each time
it happens; how much a subscription is wanted; the two layers of time; telling the
subscriber and the smaller size; the paused tile; the conductor's part; going back up.

### Video through the khef, as built

- **The plan gives each khef, for every video track, where it comes from**: this node's
  own camera, or the neighbour on the way to it (to begin with, along the audio tree).
- **Asked for hop by hop.** A khef that wants a track, for its own client or for a
  neighbour that asked, sends one `SUBSCRIBE` to that neighbour, however many want it.
  When the last has gone it gives the subscription up. The node with the camera is told
  when the first watcher arrives and when the last has left, and makes pictures only
  between the two.
- **Groups are passed on as they arrive**, a stream per group, the bytes unchanged.
  A group cannot be joined part way: whoever begins to watch gets the next group.
- **Key frames**, as the draft has it (`NEW_GROUP_REQUEST`, section 10.2.19 of draft
  20). A request names the group that must answer it: the largest group the asker
  knows of, plus one (zero if it knows of none). A khef passes a request on only if it
  has not seen that group begin and has not already asked for it, so any number of
  requests for the same key frame become one, with no clock in the khef. A new
  subscription asks for a new group; so does a watcher that joins a track already
  flowing or loses its place. How often key frames are made is the encoder's alone to
  limit.
  Each track served this way carries the draft's `DYNAMIC_GROUPS` property, which is
  what permits the request; a session does not ask where it is absent.
- **When the way changes** the khef gives up its subscription and makes another by the
  new way; groups part way through are given up all the way down. A neighbour that
  refuses (it may not have the plan yet) is asked again after half a second. A track
  is never sent back to the neighbour it comes from.
- **The node** works out each track's way from the plan (the neighbour on the audio
  tree towards whoever makes it) and lists every video track in the plan for the
  application.
- **The two ends** (`sdk::video`). The sender codes nothing while nobody watches. A key
  frame begins a group and ends the one before; one that is asked for is made at once
  if enough frames have passed since the last, and otherwise when they have (the
  encoder's own limit; the khefs have none). Groups are numbered from nought, and go
  on from where they were when an encoder is replaced: a track's name holds its
  participant's id, which is made from a key that is new each time a node starts, so
  a node that starts again has new tracks and no khef remembers numbers for them.
  (Should a node ever keep its key from one run to the next, the track's name would
  need a number of its own for each run.) The
  receiver shows each frame as soon as it is whole; a group that begins takes the place
  of an older one still arriving; when a frame cannot be decoded or a group is given up
  it asks once for a group after the one it lost, and waits for one to begin.
- **One node's pictures** (`sdk::pictures`): the sender and the receivers behind the
  engine's door for video, for one thread to turn. A participant offers a camera by
  naming it in its options; nothing is coded until somebody watches.
- **The app and the bot**. The app's pattern (`--pattern`) is offered as its camera
  and coded at 500 kbit/s, with a key frame by itself every ten seconds. It watches
  the first video track in the plan that is not its own and shows it under its
  owner's name; the details give, each way, the rate, the frames a second and the
  time to code one. Coding runs on a thread of its own, which looks for work every
  5 ms. The bot does the same with `--pattern`, in its speaker's colour.
- **Not yet**: keeping the group in hand for whoever joins part way; the new way taken
  up before the old is dropped; anything for a link that cannot keep up.

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
  **The scale**: louder is higher, 127 is a full-scale signal and each step down is one
  decibel, so a level `L` is an RMS of `L - 127` dBFS (the scale of RFC 6464 turned over).
  Speech into a well-set microphone is around 95 to 110; a quiet room around 60 to 70.
  Opus in-band FEC is on from the start; DRED is added when loss calls for it (section 14).
  One object per group, in datagrams.
- **Selection rule** (per khef, separately for each overlay link and for its own client; the
  candidates for a link are the tracks not learned from that link). The khef holds a selected
  set of N tracks, each with the time it entered the set. On each packet arrival, with every
  track's level taken from its latest packet:
  1. Rank the candidates by level.
  2. A track among the N loudest that is not selected, and is above the minimum level, replaces
     the lowest-ranked selected track that is not itself among the N loudest and is past its
     hold. If there is none, it is not added.
  - **The hold** runs from the moment a track enters the set: for that long it cannot be
    replaced. It is for the listener: a track that came and went within a few packets would
    be heard as a fragment of sound, and a slot flapping between voices of similar loudness
    as a run of them. It applies to every track that enters, loud or quiet, into a free slot
    or another's; even a quiet one may carry noise a listener would hear start and stop.
    It is all that slows a switch: once it has passed, a track that no longer ranks goes the
    instant someone who does needs the slot.
  - **So the first N heard stay for the hold**, even if a louder voice starts in the same
    instant: with four starting together and N = 3, the third loudest can be 260 ms late if
    the quietest was heard first.
  - **Khefs along a path do not add to each other's hold.** A track enters the set at each
    of them at the same moment (give or take the link delay), so its hold ends at each
    together. Measured with N = 3: a voice that has to wait out another's hold is 260 ms
    late for every listener, however many khefs away.
  - **Fast in**: a speaker who ranks gets in on their first speech packet unless every track
    that does not rank entered within the hold. Slots are never
    emptied, so when everyone is silent the set does not change, and quiet background noise is
    consistently in or out.
  - No timers: the rule runs on packet arrival, so it adds no delay.
  - **Level decay**: a track's level is held for two packet intervals after its latest packet,
    then decays linearly to zero at 100 ms, so a crashed talker or a lost end-of-speech packet
    cannot hold a slot. Computed on arrival of any packet; still no timers.
  - Hold time (250 ms, from spike 4) and minimum level are profile constants.
  - **The minimum level is 80** (-47 dBFS): over a quiet room, under speech. Like the hold
    time it has not been tuned by ear.
  - **Known weakness: voices of similar loudness can swap for a moment, and the swap then
    lasts the hold.** A voice is ranked by its latest packet, and a level moves several
    decibels between packets as syllables rise and fall. So a quieter voice whose new
    packet is compared with a louder voice's packet from a frame or two earlier can outrank
    it for an instant, take its slot (it is past its hold), and keep it for 250 ms. Seen
    with bots 6 dB apart. **Accepted for now**: it only shows when more than N people talk
    at once, which is a muddle whatever is selected. If it ever matters, rank on the
    highest level of the last 100 ms or so rather than on the latest packet.
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
- **Transport**: QUIC, by quinn, behind a trait that offers request streams, unidirectional
  streams and datagrams (`net::quic`). One UDP socket per node, for dialling and accepting.
  - **A connection is to a key.** Each end presents its Ed25519 key in the TLS 1.3
    handshake as a raw public key (RFC 7250, no certificates) and signs the handshake with
    it. The dialler accepts only the key it meant to reach; the accepter takes any key and
    learns which, leaving who may join to admission.
  - **Why not iroh** (which spike 1 recommended for its relay): it took the dependency
    tree from 65 packages to 379 (an HTTP client, a DNS resolver, a relay client, crates
    for browsers), needed two licence exceptions and an advisory exception, and rests on
    its makers' own fork of quinn. quinn takes the tree to 112 with no exceptions. What
    iroh gave beyond quinn was dialling by key (now ours, as above), hole punching (ours
    by design: section 3) and the relay, which is a server and a poor path for media.
  - **What every transport promises** (`net::transport`): a connection is to a key, and
    addresses are only hints; a stream delivers its bytes in order and then says whether it
    was finished or reset; a datagram arrives whole or not at all, and is dropped rather
    than queued when the path cannot take it; a close is final, is seen by both ends, and
    ends everything waiting on the connection; a writer waits when the reader is behind.
  - **The in-memory transport** (`net::memory`) keeps those promises with channels, for
    running real nodes against each other in one process. It has no delay or loss and no
    path that goes quiet without closing; those are the simulator's. An endpoint can be
    put behind a NAT: it is then reached only by those it has dialled, and a dial to it
    waits 10 s for that, which is enough to test that two such nodes get connected.
  - **Real NATs** (`scripts/nat-test.sh`): bots in Linux network namespaces, two of them
    each behind a router that the kernel makes a NAT, one in the open. With routers that
    keep a node's port (most home routers) the two behind them must end up connected to
    each other; with routers that pick a port per destination they cannot, and must still
    hear each other through the third. It runs as its own CI job.
- **The node** (`palaver_node::Node`) owns the khef and the control logic, which do not know
  of each other. Control objects go up from the khef to be checked and come down to be
  flooded; an adopted plan is carried out (dial planned peers, tell the khef where audio
  goes, close unplanned links once both ends are wired); link changes go into the node's
  report. It is sans-IO, so the simulator runs the same code the product will: every
  simulated node is a real `Node`, and what crosses the simulated links is real MoQ.
- **The engine** (`sdk::Engine`) is the part that touches things. It owns the node and is
  the only caller of it, from one task, so the khef and the control logic run in one loop
  with no locks. Small tasks around it only carry bytes (one per stream, and per
  connection one each for incoming streams, datagrams and the connection's end) and turn
  what they see into messages to that task. A received audio datagram is forwarded and
  handed to the player as slices of the same buffer.
  - **Two nodes that dial each other at once** keep the connection dialled by the lower
    participant id and close the other; both apply the same rule.
  - **Where a peer is dialled**: at every address its latest report lists, at those of
    the ticket if it is an entry point, and at every address its neighbours report seeing
    it at (each link in a report carries the address the peer's packets come from). A
    peer the plan names is dialled once an address for it is known; after 10 s without
    one the dial has failed. An address learned while a dial is under way is tried as
    well: the neighbour's report that holds a peer's public address can come after the
    dial to its private one began.
  - **Outside**: a node told to join that has no link and no plan says so
    (`Status::nobody_answers`) once everyone in its ticket has failed to answer, and
    goes on trying them all. `Engine::reach` dials nodes named in a ticket that came
    back.
- **A call** (`sdk::Call`) puts a voice on an engine. Its owner keeps a 20 ms beat and on
  each one gives it the frame captured and asks for the frame to play. Speaking is level,
  Opus, and a datagram to the engine. Listening is one jitter buffer and decoder per voice
  the engine delivers (the khefs have already chosen the loudest few), mixed. A voice that
  has been silent for five seconds is forgotten.
- **A bot** (`apps/bot`) is a whole participant with a made-up voice and ears: the same
  node, encoder, decoders and `Call` as an application with a sound card. It can say
  whose pitch is in what it was given to play, so a room of bots checks the product end to
  end by what a listener would notice. `palaver-bot create` starts a meeting over QUIC
  and prints a ticket; `palaver-bot join <ticket>` joins it from another process or
  machine; each says every second who is in the meeting and whose voices it hears.
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
  - Video uses the streaming form, and SUBSCRIBE for a single track (in the session
    since 2026-11-23). A track taken by SUBSCRIBE has its groups handed up as their
    bytes arrive, header first, to be passed to the next link as they are; only the two
    ends of the line read the frames out (`GroupWriter`, `GroupReader`). On the
    subscription's own stream the subscriber may ask that a new group begin, which is
    how a key frame is asked for, and either end may end it. A group may be abandoned
    part way by either end, which costs that stream and nothing else. Not yet bounded:
    how much may wait to be sent to a slow subscriber.
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
  non-member MoQ relay (a public relay network) carries media. Sender signatures
  for attribution are not part of secure-objects and would be our addition if wanted.

## 14. Media pipeline

No libwebrtc. The pipeline is assembled from parts behind our own traits in `media` (spike 5):

- **Audio processing**: `sonora`, a pure-Rust port of WebRTC's echo canceller, noise suppression
  and gain control. Platform voice-processing units on Apple/Android where better.
  - `media::process::Processor` is shown every frame on its way to the speaker and cleans
    every frame from the microphone; each of echo, noise and level can be turned off. It
    is for a voice at a microphone: a track of music or shared sound goes without it.
  - Measured in a made-up room (the speaker heard at the microphone 30 ms later at half
    the level): 40 dB of the echo taken out after two seconds, with one channel of
    playout or two; steady hiss down by more than 10 dB; a quiet voice up by 14 dB.
  - An echo canceller learns the room from what is played, and learns almost nothing
    from a steady pitch: 3 dB taken out of the bots' made-up voices. So the bots' voices
    are no test of it, and it is tested on broadband sound in syllables.
  - *Headphones.* With no echo to find, the canceller must notice and stand aside, or it
    takes whoever talks at the microphone for an echo of the others. Its default way of
    noticing took eight seconds of the others talking, and until then let through a
    millionth of the talker (measured). **Decided: the other way it offers** (a
    two-state model, `TransparentModeType::Hmm`), which stands aside at once when the
    others have talked alone first, with the same 40 dB taken out in the room. Still
    lost: about two seconds of a talker who starts at the same moment as the others, at
    the start of a call.
  - Not tested: a real room, and drift between the speaker's clock and the microphone's.
- **Audio codec**: libopus 1.6 (C, built from source) with DTX and in-band FEC; DRED when
  loss calls for it.
  - Wrapped by `palaver-opus`, the one crate allowed `unsafe`: speech at 48 kHz in
    20 ms frames, in one channel or two; decode, decode from the next packet's FEC, decode from a later packet's
    DRED, conceal.
  - **FEC and DRED are paid for out of the bitrate, and at 32 kbit/s there is not room for
    both.** How far back a packet's DRED history reached, measured with libopus 1.6.1 on a
    voice-like test signal:

    | Bitrate | Loss planned for | FEC and DRED | DRED only |
    |---|---|---|---|
    | 24 kbit/s | 15% | none | 95 ms |
    | 32 kbit/s | 15% | none | 135 ms |
    | 32 kbit/s | 30% | 95 ms | 215 ms |
    | 48 kbit/s | 15% | 95 ms | 375 ms |
    | 48 kbit/s | 30% | 415 ms | 495 ms |
    | 64 kbit/s | 15% | 175 ms | 495 ms |

    So at 32 kbit/s it is one or the other. **Decided: start with FEC only.** DRED is
    turned on when a link shows the bursts of loss it is for, which belongs with the later
    work on adapting to the network (and means FEC off or a higher bitrate when it is on).
  - The jitter buffer cannot know how far a packet's DRED reaches; it asks, and the decoder
    answers "not that far" (then conceal).
- **One voice, without the devices** (`media::voice`): `VoiceSender` takes 20 ms of
  samples and gives at most one packet, with its level; nothing is sent for a frame the
  encoder marks as silence, and the frame counter advances all the same. `VoiceReceiver`
  holds a jitter buffer and a decoder for one remote voice and gives 20 ms to play on every
  frame interval: the frame, a lost frame rebuilt from the next packet, or the decoder's
  guess. Voices are summed and passed through a limiter that leaves ordinary levels
  untouched and squeezes what would clip (`media::mix`).
- **Channels** (decided 2026-11-21). Samples of two channels are interleaved; a frame
  says how many it has by its length.
  - *A microphone is sent in one channel.* One voice at one microphone has no left and
    right; two channels would cost bitrate and make echo cancellation harder for nothing
    a listener hears.
  - *A source that has two is sent in two, when its owner asks*: shared computer sound,
    music, a stereo interface. Voice processing is off for such a track. The encoder's
    settings say which (`EncoderConfig::channels`); nothing on the wire needs to, because
    an Opus packet says what it holds and a decoder gives either from either.
  - *What is played has two channels wherever the device has.* Each voice is decoded on
    its own (the khefs forward, they do not mix), so the listener's node decides where
    each is heard. **Planned: a voice is heard where its picture is** on the screen, left
    to right; a track sent in two channels is played as sent. The app gives each voice
    its place; the mixer applies it.
  - Measured with libopus 1.6.1: FEC, DRED and concealment all fill a lost frame of two
    channels as they do one.
- **Between a device and the frames** (`media::pcm`, `media::resample`): a device has its
  own rate, channels and block sizes. `Capture` and `Playback` convert both ways, each
  channel through its own rate change. A device with one channel gets two averaged; a
  device with more than two uses its first two.
- **Made-up voices** (`media::synthetic`) for bots and tests: a pitch with harmonics and a
  syllable rhythm, which Opus treats as speech. Each voice has its own pitch (eight within
  one octave), so a listener can tell whose voice is in what it plays after encoding, loss
  and mixing, by measuring that one frequency.
- **Audio I/O**: `cpal` (WASAPI, ALSA or PulseAudio, CoreAudio), in `palaver-sound`, the
  one crate that touches a sound device. Only the app depends on it: bots and tests build
  without it.
  - `Microphone` gives frames and `Speaker` takes them: the system's default devices, in
    the format the system suggests. Devices differ in rate, channels and sample type
    (16-bit at 44.1 kHz is common), so everything is converted to 48 kHz float.
  - The device is asked to work in pieces of 10 ms, and opened with what it prefers if it
    refuses. Left alone, PulseAudio buffered about four seconds of playback (measured
    through WSLg), which is four seconds of delay.
  - At most 200 ms may wait for the speaker; beyond that the oldest is dropped, so what
    is heard cannot fall ever further behind.
  - The device calls back on its own thread. That thread and the owner's meet at a lock
    held only while samples are copied. To revisit if it shows up as glitches: a
    lock-free queue.
  - To hear it: `cargo run -p palaver-sound --example loopback` plays the microphone
    through Opus and a jitter buffer to the speaker. With `other` it plays a made-up
    other voice instead and saves what a call would send, which is the way to judge
    echo cancelling on real devices: played back to its own talker, a canceller takes
    the talker for an echo and drops words, and that says nothing about a call.
  - Heard on real devices (2026-11-21): clean on Windows (WASAPI, a USB headset at
    48 kHz float). Through WSLg it is not: the microphone delivered 2 to 10% more sound
    than real time and the speaker took 3% too little, so words were lost whatever the
    code did. WSLg is no place to judge sound.
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
- **Video codec, as built** (`media::video`): two traits of our own, `Encode` and
  `Decode`. A coded frame is a whole frame in byte-stream form and is either a key
  frame, which a decoder can begin at, or not; a key frame comes when asked for and,
  if set, every so many pictures; the encoder may let a picture go to hold its rate.
  `SoftwareEncoder` and `SoftwareDecoder` are openh264 through the `openh264` crate,
  which builds it from source and needs no unsafe code of ours. Sizes must be even.
  Measured with the moving pattern, 640 × 360 at thirty a second (2026-11-23): on a
  desktop 1.3 ms to encode, 0.2 ms to decode and 0.5 ms to make ready for the screen;
  on a handheld 2.8, 0.6 and 1.1 ms. That is without the assembly routines
  (those need `nasm` at build time, which nothing installs yet; to be added where the
  numbers on real pictures call for it).
- **Video codec**: our own encoder/decoder interface. openh264 (software) is the first backend;
  **hardware backends follow early (phase 2)**, because a node must keep CPU for its khef.
  Media Foundation on Windows first, then VideoToolbox, VAAPI/NVENC, MediaCodec. `moq-video`'s
  backends (MIT/Apache) are the starting point to copy from, without its `moq-net`/`hang`
  dependencies. Software encoders may skip frames to hold a bitrate; the pipeline tolerates it.
- **Pictures** (`media::picture`): a frame of video is kept as cameras and codecs keep
  one (I420: brightness for every pixel, colour for every two by two), and turned into
  what a screen wants in whole-number arithmetic. `Pattern` makes moving pictures for
  bots and tests, a colour to each speaker.
- **Video capture**: behind a small crate of our own, as sound is, so that what is
  underneath can change. Looked at (2026-11-22): `scap` captures screens only;
  `nokhwa` does cameras on all three systems but looks unmaintained (last release
  line old, many open issues), so it is at most a first backend that we expect to
  replace; `moq-video` has camera backends for all three (some 450 to 800 lines each)
  that cannot be depended on, because the crate brings its own network stack, but can
  be copied and owned. Expect to maintain capture ourselves either way.
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

What there is so far, in three layers:

- `sdk::Engine` runs one node (its khef and its control logic) over a transport.
- `sdk::Call` puts a voice on it: 20 ms of samples in, 20 ms out.
- `sdk::Participant` is a node on a network with a key of its own, started together with
  its call, which is what an application or a bot begins with: over QUIC or in memory, it
  makes tickets, joins by them, answers a ticket that came back, and finds out what
  network it is on. It works through an `sdk::Controller`, a handle on the running node
  that can be cloned and kept apart from the call, so that the call can go to the thread
  that serves the sound card. The bot is a participant with a made-up voice and ears;
  the app is one with a sound card.

**A participant with a sound card** (`apps/harness`):

- `voice::Voice` joins a call to a microphone and a speaker. **The speaker keeps time**:
  each turn makes frames only until a set lead (20 ms) waits for the device, and sends on
  whatever the microphone has gathered. A timer beside the device would be a second
  clock; with the speaker as the clock, a device that takes 3% too little is simply
  given 3% fewer frames, and nothing is thrown away (tested on a made-up speaker; seen
  on WSLg's, which is that slow).
- `live::Live` runs one on a thread of its own, against the system's devices, and keeps
  a meter for whoever shows it.
- `session::Session` is the meeting behind the window: the node, its network and its
  sound on a thread of their own, taking orders (start, join, reach, mute, leave) and
  keeping a view up to date. The window only draws the view and sends orders, so it
  never waits on the network.
- `check::MicCheck` listens to the microphone outside a meeting, for the meter on the
  first screen. `check::Ear` judges what a microphone gives (nothing, cleaned by the
  system first, faint, good; and whether it has much noise of its own) by its sound
  alone; the check runs one, and so does the sound thread during a meeting, where a
  microphone found noisy has its noise turned down harder from then on.
- `picture::Slot` holds the newest picture of one tile: whoever makes pictures puts
  each in, the window takes the newest when it draws and is woken for it, and one not
  taken in time is dropped. `--pattern` shows a moving pattern as this machine's own
  picture, until there is a camera, and `--coded <kbit/s>` shows it also as it would
  be sent: through the encoder and the decoder, with the rate and the cost of each in
  the details. Making a 640 × 360 picture ready for the screen
  on the processor takes about half a millisecond, which is fine for a few tiles and
  is what the choice of renderer is to be measured against for many.
- `history::History` keeps the voices and links as they are and two minutes of the
  numbers worth watching, one value a second, for the details to draw over time.
- **The window** (`ui`, egui through `eframe`, drawn with OpenGL): run the program with
  no arguments. Outside a meeting: a name, the microphone and its meter, start a
  meeting, or paste a ticket and join. Inside: the people in a circle round a fire that
  burns higher the more voices are heard, mute, "bring someone in" (a ticket to the
  clipboard), leave, both ends of the ticket coming back, and the details, which slide
  out beside the meeting: this machine, each link and each voice, as numbers and as
  lines over time. The colours are the "Ember" look. Still to come from the designs:
  the grid and the map, the fonts, who is talking.
- **Threads.** What runs where in the app, and what each may wait on:

  | Thread | What it does | Waits on |
  |---|---|---|
  | Window | Draws, takes clicks, reads a copy of the view | The display |
  | Session | One async runtime: the engine (control, and forwarding for others), QUIC, timers, bringing the view up to date | The network, timers |
  | Sound | Encodes, decodes, echo cancelling, jitter buffers | The speaker taking sound |
  | Device callbacks (two) | Copy samples in and out | The sound card |

  They meet at two locks, each held only for a copy: one round the view, one round each
  device's samples. The rules:
  - The window only reads the view and sends orders. It never waits on the network or
    the sound.
  - The sound thread waits on nothing but the speaker.
  - Forwarding other people's media must not wait behind control work (a plan being
    worked out, signatures being checked). Today both are on the session thread; whether
    they need threads of their own is to be decided with numbers from meetings of fifty.
  - **Measure before changing.** The details in the window show, each as the worst of
    the last five seconds: the longest the sound thread went between turns (about 10 ms
    is right), the most frames the microphone had waiting at one turn (one is right;
    more means our frames leave in bursts, which the far end's jitter buffer takes for a
    bad network), how late the session thread's timers ran, and for each voice played
    the delay its jitter buffer aims for and what became of its frames.
  - Expected next, once the gauges say so: the sound thread at the system's priority for
    sound.
- `palaver-harness shot <file.png> [name]` makes the window take a picture of itself
  and close: of the first screen, or of a meeting started under a name. It is how the
  screens are looked at without a person at them. The PNG is written by a few lines of
  our own (`png`), stored without compression.
- The program is also a call from the command line: `palaver-harness create`, or
  `join <ticket>`.
- **What the window costs.** `eframe` brings the app from 103 packages to 221 on Linux
  (95 to 173 on Windows); the bot and everything under it are untouched. The OpenGL
  renderer was chosen over the default (wgpu) because it is fewer packages.
  **Decided 2026-11-21: OpenGL while the window has no video; expect wgpu when it
  does.** wgpu draws with what each system supports best (Metal, DirectX 12, Vulkan;
  Apple has deprecated OpenGL), and it can take a frame from a hardware decoder
  without a copy through the processor, which twenty-five tiles of video will want.
  The change is a feature flag and the drawing of video tiles, which does not exist
  yet. To be settled by measuring, with real video. Three licences come with it and are allowed only for the crates that
  carry them (`deny.toml`): the Boost licence for the Windows clipboard, and two font
  licences for egui's built-in fonts.

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
2. **First-contact NAT traversal** without an operated server (section 3, Reachability; a
   symmetric NAT with no reachable member is not covered).
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
| Audio | Top-N selected by khefs over a pre-wired overlay of two loop-free trees sharing no middle node (one tree in phase 1); loudness-ranked, a track is held for a short time after it enters; all audio tracks announced at join; DTX, FEC and DRED |
| Rendition switching | Receiver-side by default; khef-side track swap allowed if the rate-control spike finds a clean way |
| Local client↔khef | In-memory MoQ-formatted messages; no sockets, QUIC or crypto |
| Network edge | Conformant to the pinned draft for our profile |
| Draft / codec | Draft 22; our own codec in `moq-wire` for our profile; `moqtap-codec` as the test oracle |
| Transport | quinn; keys proven in the TLS handshake; members introduce; no relay server |
| Link preference | Direct, then via a transit khef |
| Naming | **khef** = the MoQ relay in each node |
| Infrastructure | Public STUN-like services fine; no relay server; public MoQ relay network later |
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
