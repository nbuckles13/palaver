# Palaver — alternate design: MoQ-native

Status: draft for discussion, 2026-11-15. An alternative to the media, routing and conductor parts
of [02-design.md](02-design.md) ("design A"), which is paused. Everything not mentioned here is
unchanged: discovery and admission, single conductor with succession, link-state reports,
flooding, leave/crash goals, SDK, operability, testing, development flow.

MoQ facts below are from draft-ietf-moq-transport-23 unless marked otherwise; see
[04-moq.md](04-moq.md).

## 1. The idea

Every node's hub is a small **MoQ relay of our own**. A client speaks MoQ Transport (MoQT) to a
relay — by default its own, in-process. Relays speak MoQT to each other. The conductor controls how
relays are wired. MoQT explicitly treats "a coordinated set of relays as a single relay" and leaves
its insides unspecified, so the conductor, the routing and the audio selection are ours to design
without leaving the standard.

```
 node A                         node B                         node C
┌──────────────┐               ┌──────────────┐               ┌──────────────┐
│ client       │               │ client       │               │ client       │
│   │ MoQT (in-proc)           │   │                          │   │          │
│ relay ◄──────┼── MoQT/QUIC ──┼─► relay ◄────┼── MoQT/QUIC ──┼─► relay      │
└──────────────┘               └──────────────┘               └──────────────┘
        ▲ control (conductor ↔ relays): ours, carried as MoQ tracks
```

## 2. What changes relative to design A

More than media format, routing and conductor logic — four further things move:

| Area | Design A | MoQ-native |
|---|---|---|
| Client attachment | Client connects to several hubs; conductor directs sends/receives per hub | Client talks to one relay (its own). **All multi-path, migration and failover happen relay-to-relay**; the client never migrates |
| Node roles | Some nodes are hubs | Every node runs an **edge relay** for its own client; the conductor additionally uses some as **transit relays**. "Hub" now means transit |
| Receive model | Client declares slots; conductor/hub decide who fills them | Client **subscribes to named tracks**. Layout is the client's decision; the conductor bounds it by policy (limits per subscriber) |
| Encryption | ADR-0036 style (own header, SFrame construction, signatures) | MoQ **secure-objects** draft (SFrame-derived; authenticates track name, group/object ids and immutable properties), still at -01 |
| Rate adaptation | Hub picks a layer per subscriber | **Subscriber-driven**: renditions are separate tracks; the receiving edge relay switches subscription. Relays shed load by priority |
| Control dissemination | Own flooding protocol | Can ride MoQ: reports and plan as tracks with a fixed flood rule (§6) |

The first row is a real simplification: a client is attached to exactly one relay for its whole
life, so graceful leave and crash handling involve only relays.

## 3. Naming

MoQ identifies a track by a **namespace tuple** plus a **track name**. There are **no wildcards in
names**; the only pattern mechanism is **namespace-prefix matching** (`SUBSCRIBE_NAMESPACE`,
`SUBSCRIBE_TRACKS`). So `*-main-audio-opus` is not expressible, but putting the fields that need
wildcarding *last* in the tuple is:

```
namespace = ( meeting, kind, source, participant )      name = rendition
            ( m-7f3a , audio, main  , user1       )  /  opus
            ( m-7f3a , video, main  , user1       )  /  1080p | 360p | 180p
            ( m-7f3a , video, screen, user1       )  /  1080p
            ( m-7f3a , ctl  , report, user1       )  /  0
```

- "All main audio in the meeting" is the prefix `(m-7f3a, audio, main)`.
- `meeting` is an opaque value derived from the meeting secret; `participant` is the node's short
  id. Nothing human-readable needs to be on the wire.
- After subscription a track is referred to by a numeric alias, so names cost bytes only at setup.
- Which tracks exist comes from the roster/plan (or a per-participant catalog track).

## 4. Video: subscribe by name, routed hop by hop

1. A client sends `SUBSCRIBE (m, video, main, user1)/360p` to its edge relay.
2. The relay resolves it from a **routing table supplied by the plan**: for each publisher (or
   namespace prefix), the next-hop relay(s) toward it. If the relay already carries the track it
   attaches the subscriber; otherwise it sends `SUBSCRIBE` to the next hop, and so on until the
   publisher's edge relay, which subscribes to its local client.
3. Objects flow back along the same path. A publisher's client produces a track only while its
   relay has at least one subscriber for it.
4. Keyframes: `NEW_GROUP_REQUEST`, forwarded upstream and deduplicated per hop (standard).
5. A subscriber joins at the next group start; one group per QUIC stream.

**Decision: relays resolve subscribes from a routing table; the conductor is not in the path.**
(Alternative considered: every subscribe forwarded to the conductor. Rejected because it adds a
conductor round trip to every gallery page, speaker-follow and rendition switch, and stalls new
subscriptions during conductor succession.)

**The table is longest-prefix-match over the full track name, not per participant.** A default
entry per publisher covers the common case; the conductor adds more specific entries wherever it
wants finer control:

```
(m, video, main, user1)            → via relay B          default for user1's camera
(m, video, main, user1) / 1080p    → via relay D          high rendition takes the fat path
(m, video, screen)                 → via relay B, C       all screen shares, two next hops
```

- Granularity is therefore **per track when the conductor wants it**, at the cost of table
  entries only where the default is not good enough.
- Tables are **per relay**, so two subscribers at different relays already get different paths.
- An entry may also carry a **cap** ("no rendition above 360p via this hop"), which is how the
  conductor expresses link budgets.
- What is given up versus conductor-in-path is only the *request-time* decision for one specific
  subscription. Entitlements and per-subscriber limits in the plan cover admission; routes and
  caps in the next plan cover optimisation.

**Default routes mean media flows on subscribe.** The conductor installs a per-participant
default route when a participant joins, so no conductor action is needed per subscription.

**Join race.** A participant's roster entry and its default routes travel in the *same* plan
generation, so a client cannot learn of a publisher from a plan that lacks its routes. The
remaining race is between relays that have applied different generations: a relay receiving a
subscribe it has no route for yet **holds it pending** (bounded) and resolves it when the route
arrives, rather than refusing. A short delay for a brand-new participant is acceptable.

**Multiple conductors, later.** Routes are separable by publisher, so several conductors could
each own a disjoint set of publishers. What is *not* separable by publisher is shared state: link
and relay budgets, the relay peering graph, the audio overlay, roster/admission and succession.
Splitting conductors therefore also needs budgets partitioned between them (or one coordinating
layer for the shared parts). Deferred with conductor scaling generally.

## 5. Audio: top-N over a pre-wired overlay

- A client sends one `SUBSCRIBE_TRACKS` for prefix `(m, audio, main)` with a **top-N filter** on
  an `audio_level` property. Its edge relay answers with `PUBLISH` for the tracks currently
  selected and ends/starts tracks as the selection changes. The client learns who is speaking from
  the track names it is handed.
- **Top-N is not in the transport draft.** Draft-23 has only range filters on properties. The
  top-N track filter exists in Cisco's individual "MOCHA meetings" draft (-00), which describes
  exactly this conferencing pattern. We implement it as a relay extension shaped like that draft,
  so we converge if it is standardised.
- **Between relays** the conductor pre-wires an audio overlay (a loop-free tree, or two for
  redundancy) as standing top-N subscriptions. Each relay ranks all audio tracks it sees — its
  local client's and those arriving from neighbours — and forwards on each overlay link the top N
  it did not learn from that link. Same convergence argument as design A: a meeting-wide top-N
  track is top-N at every relay that sees it.
- Audio objects are one per group, sent as datagrams; `audio_level` is a relay-readable object
  property. Declared levels are untrusted and bounded by conductor-set priority rules.

## 6. Conductor and control

- **Plan contents**: relay peering graph; video routing table; audio overlay and N; budgets and
  per-subscriber limits; entitlement rules; contingency next-hops; successor list.
- **Control as MoQ tracks.** Each node publishes its report on `(m, ctl, report, <node>)`; the
  conductor publishes the plan on `(m, ctl, plan)`. The `(m, ctl)` prefix has one hard-wired
  routing rule that needs no plan: forward to every neighbour, dedupe by (track, group, object).
  That is the flooding of design A, expressed in the same machinery as media. Joiners fetch the
  latest plan group from whichever relay they reach.
- **Conductor ↔ relay semantics** (what a plan means, applied-generation echo, contingencies) are
  ours. Nothing in MoQ constrains them.

### Links, joining, and how the conductor learns who exists

Signalling above MoQ is needed for joining and for running the meeting. Almost all of it rides
the `(m, ctl, …)` tracks; only admission comes before MoQ.

**Two rules for links**

- **Any relay accepts a connection from anyone who passes admission.** Such a link always carries
  the control namespace. This is how a newcomer gets in; it does not wait for the conductor.
- **Media flows on a link only where the plan says so.** "Relays connect when told to" applies to
  *planned peerings* for media, which the conductor adds on top.

**Join sequence**

1. **Find a member.** Rendezvous (ticket, mDNS, DHT) yields addresses of one or more members. The
   very first node has nobody to find; it creates the meeting and is conductor.
2. **Connect and admit.** QUIC connection to any member (the *entry relay*), then the password
   handshake (PAKE) on that connection, before any MoQ message.
3. **Open MoQ, control only.** The joiner subscribes to the `(m, ctl)` prefix and receives the
   current plan and roster, then publishes its own **report**: identity, name, capabilities,
   candidate addresses, which members it can reach.
4. **The conductor learns of it from that report**, which floods over the control namespace like
   any other. That is the answer to "how does the conductor know which relays are out there":
   every node announces itself over the link it joined through; the roster is the set of live
   reports. No direct conductor connection is needed.
5. **The conductor issues a plan** containing the new roster entry, its default routes, and its
   planned peerings ("connect to relays X and Y").
6. **Peerings form.** Both ends see the same plan generation; addresses come from reports; the
   entry link carries what is needed to coordinate hole punching. Each new link runs admission
   too.
7. **The entry link** is kept if the plan uses it, otherwise closed once the planned peerings
   are up (make before break).

**What rides the control namespace** (all ours, all MoQ tracks with the flood rule): reports,
plan, roster; moderation commands (mute, kick, lock); hole-punch coordination; later, key
distribution.

## 7. Leave and crash

- **Transit relay leaves**: conductor issues a plan with new routes; each downstream relay
  subscribes via the new next hop while still receiving from the old, dedupes by
  (track, group, object), and drops the old path when the new one is live. MoQT describes this
  switchover and provides `GOAWAY` with a timeout as the drain signal. Clients see nothing.
- **Transit relay crashes**: downstream relays apply the plan's contingency next-hop on detection.
  Audio already arrives over two overlay trees, so it has no gap; a video track resumes at the
  next group from the alternate path.
- **A node leaves or crashes**: its edge relay and client go together. Its tracks end; if it was a
  transit relay, the two cases above apply.
- **Conductor leave/crash**: as in design A.

## 8. Congestion

Rendition choice is **purely subscriber-driven**: a relay never substitutes one track for another.
A relay may still *drop* (by priority) when a link cannot carry what was asked of it.

Congestion can sit on any hop, e.g. publisher relay → R2 → R3 → receiver relay with the bottleneck
at R2→R3. Only R2 sees it directly (its QUIC send state toward R3). The chain of reactions:

1. **R2 sheds by MoQ priority** on that link (see below); groups past their delivery timeout are
   reset. This protects audio and keeps the link usable immediately.
2. **R2 marks.** Objects it forwards on the congested link carry a hop-set, relay-mutable
   "congestion experienced" property (the ECN idea at object level), and R2's report carries the
   link's utilisation and loss.
3. **The receiver backs off.** The receiving client (with its edge relay, in-process) sees marks —
   or, failing that, late/missing groups — and switches to a lower rendition. Its relay
   unsubscribes the higher one; if nobody else behind R3 wants it, the unsubscribe propagates and
   the congestion eases. Marks make this faster than waiting for underruns, and tell the client it
   is a path problem rather than its own downlink.
4. **The conductor reroutes or caps.** From reports it can move tracks to another next hop
   (per-track entries make "1080p goes around, 360p stays" possible) or publish a cap for that
   link so subscribers stop asking for what cannot be delivered. Sometimes no better route exists;
   then the cap is the answer.
5. **Probing back up** is the subscriber's job, guided by the published caps to avoid oscillating
   against a known bottleneck.

### Priorities across relay links

MoQ orders delivery by subscriber priority, then publisher priority, then group order. Between a
client and its relay that is unambiguous. On a relay↔relay link it is not: one upstream
subscription from R3 to R2 stands for many downstream subscribers with different priorities.

- **Transit links use conductor-set priority classes carried in the plan**, matched by the same
  longest-prefix rule as routes (e.g. audio > screen share > low video renditions > high video
  renditions; a pinned or host stream can be raised). A relay uses the class as the *subscriber
  priority* of its upstream subscription.
- **Publisher priority is passed through unchanged** and breaks ties within a class.
- **A receiver's own priorities apply on its last hop.** Its edge relay serves one client, so it
  can pass that client's subscriber priorities upstream as they are, bounded by the class.
- Result: shedding is consistent meeting-wide on shared links, and personal on the link that
  serves only one person.

### Uplink congestion (publisher side)

The publisher's edge relay is the QUIC sender on the uplink, in-process with the encoder, so it
sees uplink congestion first-hand and can act at the source rather than drop after encoding:

1. Lower encoder bitrates within the current renditions.
2. If that is not enough, **stop producing a rendition**: the one that frees the most uplink for
   the least loss of value — normally the highest; a rendition nobody subscribes to is already
   not produced. Never the lowest rendition, never audio.
3. Announce it: the rendition's track ends and the catalog marks it unavailable, so subscribers
   move down instead of waiting. Restore it when the uplink recovers, with hysteresis.
4. The conductor sees the uplink state in reports and can reduce how many next hops the publisher
   feeds directly, or cap that publisher's top rendition in the plan.

Open detail: fairness among several subscribers sharing a congested hop (who backs off first).
Priority shedding decides it in the short term; caps decide it in the long term.

## 9. Remote clients

A client speaks MoQT to *a* relay; nothing requires it to be the local one. Two cases.

### Thin Palaver clients (our SDK, a web client we write)

Same join sequence as a full node, minus the relay: connect to a reachable member, run the
password handshake, open MoQ, read the plan, publish a report (marked "no relay"), then publish
and subscribe media through that one relay. They are ordinary participants with the same password
and, later, the same frame encryption. We control both ends, so the signalling is ours.

### Third-party MoQ clients: the meeting URL (future)

Not planned work: no third-party MoQ clients of note exist yet. Recorded so the design does not
preclude it.

A stock MoQ client cannot run our handshake or read our control tracks. Instead, a member (the
host, to start with) can **generate a MoQ URL** — relay address plus an unguessable secret — that
can be pasted into any MoQ client. It is a bearer secret: whoever holds the URL is in.

The entry relay acts as that client's **proxy participant**:

| Concern | How the entry relay handles it |
|---|---|
| Admission | The secret in the URL (as path or MoQ authorization token) stands in for the password. Each URL is individually revocable, optionally expiring, and scoped by the host (view-only, or allowed to publish) |
| Presence | The entry relay publishes a report on the client's behalf; it appears in the roster as an external participant "via <host>" |
| Discovery | A standard catalog track lists what can be subscribed to |
| Control | None needed: the client only subscribes and publishes; the entry relay enforces scope and limits |
| Audio | Top-N is our extension. The client subscribes to explicit audio tracks, or the entry relay may offer a standard-looking track set it fills with the current top N |
| Frame encryption (later) | The entry relay is a member and holds keys, so it can decrypt for the external client and encrypt what it publishes. The host chose to trust the URL holder; the trust boundary is the entry node |
| Leaving | Revoking the URL or the entry node leaving ends the session |

Practical limits, to be tested rather than assumed:

- The entry node must be **directly reachable** (public address or mapped port); stock clients do
  not use our hole punching.
- **Certificate trust.** By default a relay's identity is a self-generated key, which a stock
  client would have to pin or skip verifying. That is a default, not a constraint: a node can be
  given a CA-signed certificate (public CA for a named host, or an organisation's own CA that its
  browsers and clients already trust), in which case stock clients and browsers connect normally.
  So the entry relay's certificate should be configurable.
- Same **draft version** and a usage within our profile.

**Conformance** is a goal on the *network* client↔relay edge: exact for the messages in our
profile, refusal of the rest as the draft specifies. Relay↔relay links additionally carry our
extensions.

## 10. Our relay: scope

"Very light, efficient, fit for purpose" means a **profile** of MoQT, not the whole draft (which
says a relay must implement every message):

- **In**: session setup; `SUBSCRIBE`, `PUBLISH`, `SUBSCRIBE_TRACKS`/`SUBSCRIBE_NAMESPACE`,
  `REQUEST_UPDATE`; subgroup streams and datagrams; priorities and delivery timeouts;
  `NEW_GROUP_REQUEST`; `GOAWAY`; immutable/mutable properties; property range filters; our top-N
  extension; authorization token on setup (derived from admission).
- **Out, at least initially**: `FETCH` and caching beyond the current group (except the plan
  track); pause; anything only a CDN needs.
- **Pinned to one draft** until the RFC: the latest, or whichever a usable crate supports.
- **Relay logic is ours; the wire codec need not be.** If a small, clean crate provides MoQT
  message encoding/decoding and session state without dictating transport or routing, use it and
  pin to its draft. Candidates to evaluate in the spike: MOQtail (Rust, lists drafts 18–22 in the
  interop runner; quality not yet assessed) and the codec layer of `moq-net` (very active but
  churning, has dropped iroh, and now carries its own QUIC and crypto stack). If neither separates
  cleanly, we write the codec for our profile.
- **Built on our transport trait** (iroh or quinn QUIC, plus the in-process transport).
- **The local hop is not a network hop.** Between a client and its own relay there are no UDP
  sockets, no QUIC, no TLS and no packetisation: nothing is sent or received twice and nothing is
  encrypted twice. The two sides exchange MoQ-formatted control messages and reference-counted
  object buffers over an in-memory channel. The cost is a queue hand-off per object. QUIC and its
  crypto exist only on real network links.

Consequence: we are wire-compatible with a *profile* of one draft. A third-party MoQ client or a
CDN relay interoperates only where its usage falls inside that profile and draft version.

## 11. What we gain and give up versus design A

**Gains**

| Claim | Assessment |
|---|---|
| Not reinventing the object model, subscription semantics, keyframe request, drain, priority | Real. These are settled parts of the draft and match what we needed anyway |
| Ecosystem | Real but modest today: interop test runners, other clients and players, protocol dissectors, people who know the model. Third-party MoQ clients (recorders, players) could attach to a relay directly |
| CDN / relay-network interconnect is native | Real in shape: a CDN is another relay we subscribe to and publish to, with no gateway. In practice blocked today by draft mismatch (Cloudflare serves drafts 14/16). The inter-network draft (MiX) is an individual -00 |
| Future MoQ improvements (adaptation, resilience) | Partly. Since we write our own relay we inherit *designs*, not code. And today real-time bandwidth estimation is the least mature part of the MoQ ecosystem, so near-term this is a cost, not a gain |
| Traffic recognised as MoQ by firewalls and queues | Speculative. On the wire it is encrypted QUIC over UDP between residential addresses; middleboxes see QUIC at most. No MoQ-aware policy exists that I know of. Do not count on it |
| Structural simplification | Real, and not on the original list: clients attach to one relay; redundancy and migration are relay-only; control can reuse the media machinery; attribution comes from track names |

**Costs**

| Cost | Assessment |
|---|---|
| Draft churn | Five drafts in six months; RFC unlikely before mid/late 2027. We pin and re-pin |
| Top-N is non-standard | Our extension, modelled on an individual draft |
| No hub-composed slots | Layout and rendition choice move to the subscriber. Abuse is bounded by conductor limits rather than being unrepresentable |
| Rate adaptation is ours to build in the MoQ style | Subscriber-driven rendition switching plus relay-side priority dropping; less proven for interactive calls than SFU-side layer selection |
| Encryption on a -01 draft | secure-objects may change; frame-level signatures for attribution are not part of it |
| Baggage | Namespace tuples, varints, parameters and properties we would not have designed; modest per-object overhead |
| In-process hop | Negligible (agreed) |
| Independent velocity | Constrained wherever we want third-party interop; unconstrained inside the relay network |

## 12. Spike changes

Replaces the MoQ spike in [03-plan.md](03-plan.md):

1. Minimal MoQT profile over our transport trait: two relays, one in-process client each, one
   video track subscribed by name end to end.
2. Three relays in a line (A–B–C): hop-by-hop subscribe from a routing table; then re-route B→B′
   with dedupe and measure loss (target zero) and crash recovery time.
3. Top-N audio: 50 simulated publishers over a relay tree; verify convergence and per-link stream
   count ≤ N; measure speaker-change latency.
4. Datagram audio latency versus stream-per-object under 2–5% loss.
5. Interop probe: can an existing MoQ client (e.g. one from the interop runner) attach to our
   relay on the pinned draft?
6. Crate evaluation: can MOQtail or `moq-net` serve as wire codec only?
7. Mid-path congestion: throttle the middle link of a three-relay chain; verify shedding, marking,
   subscriber back-off and recovery.

## 13. Decisions

| Topic | Decision |
|---|---|
| Subscribe resolution | Routing table in the plan, longest-prefix match down to a single track; default per-participant routes; conductor not in the path; unroutable subscribes held pending |
| Draft | Latest, or what the best usable crate supports |
| Local client↔relay | In-memory, MoQ-formatted messages; no sockets, QUIC or crypto |
| Network client↔relay | Conformant to the pinned draft for our profile |
| Rendition switching | Purely subscriber-driven; relays drop by priority and mark congestion, never substitute |
| Priorities on relay links | Conductor-set classes in the plan; publisher priority passed through |
| Uplink congestion | Publisher reduces bitrate, then stops the rendition that frees most for least loss |
| Links | Any relay accepts admitted connections (control always); media only on planned peerings |
| Join | Rendezvous → connect → PAKE → MoQ control → report floods → conductor plans peerings |
| Thin Palaver clients | Ordinary participants via a remote relay; same password, same encryption |
| Third-party MoQ clients | Host-generated MoQ URL (bearer secret, revocable, scoped); entry relay proxies them |
| Conductor scaling | Later; multiple conductors over disjoint publisher sets is the likely shape |

## 14. Open questions

1. Is there a crate fit to be our wire codec (spike)?

3. Fairness when several subscribers share a congested hop.
4. Exact rule for "the rendition that makes most sense to remove" (bitrate freed vs subscribers
   affected).

Deferred with the third-party URL feature (not blocking): default scope of a URL (view-only or
publish), who may generate one (host only or any member), and how a relay is configured with a
CA-signed certificate.
