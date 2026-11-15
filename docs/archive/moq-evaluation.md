# MoQ evaluation: should a hub be a MoQ relay?

> **Outcome (2026-11-15):** yes. The design in [02-design.md](../02-design.md) makes every hub a
> small MoQ relay of our own, conformant at the network client edge to a profile of one pinned
> draft. The recommendation and options below record the analysis that led there; where they say
> "own protocol, MoQ-shaped" or "numeric ids", the design doc supersedes them.

Date: 2026-11-14. **[V]** = primary source read (draft text, repo, vendor docs); **[S]** = secondary;
**[U]** = unverified or inference.

## Recommendation

**Keep a conductor-driven protocol of our own, but make its data plane MoQ-shaped**, and run a
time-boxed spike of `moq-net` as a per-link library before committing.

MoQ defines well the parts we had already planned the same way (tracks, group-per-stream, datagram
objects, keyframe requests, dedupe by id, immutable vs relay-mutable metadata). It leaves out of
scope, or solves differently, exactly the parts that make Palaver distinctive: explicit
conductor-pushed forwarding, slot/last-N semantics, crash recovery in a churning mesh of peers.

## IETF MoQT (draft-ietf-moq-transport-23, 2 Nov 2026)

- **Status** [V]: still a working-group draft; went -18 → -23 in six months; implementations are
  spread across five draft versions and none lists -23. RFC before mid/late 2027 looks unlikely [U].
- **Object model** [V]: Track (namespace tuple + name) > Group > (Subgroup) > Object. One subgroup
  per QUIC stream; a group start is the join point; objects may instead be sent as datagrams.
- **Priorities** [V]: subscriber priority, then publisher priority, then group order; delivery
  timeouts.
- **Metadata split** [V]: relays must not modify objects, but may change *mutable* properties;
  *immutable* properties are end-to-end authenticated. Same idea as our publisher/relay regions.
- **Keyframes** [V]: `NEW_GROUP_REQUEST`, forwarded upstream and deduplicated by relays.
- **Migration** [V]: `GOAWAY` with timeout; the draft describes make-before-break for subscribers
  and publishers (establish on new relay, then cancel old). Relays must accept the same track from
  several publishers and should dedupe by (track, group, object).
- **Not covered** [V]: relay-to-relay behaviour is explicitly out of scope ("a coordinated set of
  relays are treated as a single relay") — so multi-hop routing and loop avoidance are not
  standardised. No slot or top-N concept in the transport draft (only individual -00 drafts). A
  conformant relay "MUST implement all messages": a large surface.
- **Related** [V]: secure-objects (SFrame-derived E2EE; authenticates track name, group/object ids,
  immutable properties, payload); MSF catalog; LOC container; Cisco's MOCHA meetings draft
  (per-device tracks, audio one object per group optionally as datagrams, video GoP per group,
  simulcast as separate tracks, relay-side top-N by audio level).

## moq-dev/moq (the main Rust implementation)

- MIT/Apache; extremely active: `moq-net` (formerly `moq-lite`) has had 66 releases since May 2026,
  a mass crate rename, and a large consolidated breaking change in July [V].
- **moq-lite** is a deliberately small subset of MoQT: pull only, one stream per request, no
  subgroups. IETF drafts 18/20/22 are also negotiated but frozen at interop scope [V].
- **Clustering** [V]: relays form whatever graph their peer lists describe; chains and partial
  meshes are supported; loops avoided with hop lists (or distance-vector routing in the unfinished
  lite-07). Routing is **automatic by cost/preference**, not explicit per stream. Timers (300 ms
  announce delay, 2 s hold-down) are CDN-oriented.
- **Seamless failover** requires lite-07 ("publisher instances", resume from first missing frame),
  which is still work in progress [V].
- **Embeddable** [V]: `moq-net` has a sans-runtime poll core and custom-transport traits;
  `moq-relay` exposes an embeddable `Relay`.
- **iroh support was removed** on 3 Nov 2026 [V]; the project now ships its own QUIC fork
  (`moq-quic`) and a weeks-old crypto library (`rind`, no licence detected). iroh-live's `iroh-moq`
  adapter is pinned to pre-removal versions.
- **Audio** [V]: datagram tracks exist but are discouraged by the author; the first-party media
  layer (`hang`) uses one QUIC stream per audio frame.
- **E2EE** [V]: its own AES-GCM profile, explicitly not SFrame.

## Other

- **Cloudflare MoQ relay network** [V]: public beta, free during preview, drafts 14/16 only, no
  FETCH/GOAWAY, datagrams undocumented. Not usable as our fallback relay unless we speak those
  drafts.
- **Browsers** [V/U]: hang's web client reaches a relay over WebTransport with certificate-hash
  pinning on Chrome/Firefox; it keeps Safari on a WebSocket fallback because of WebKit bugs. A
  browser needs the hub's UDP port directly reachable and short-lived rotating certificates.
  Realistic design: a small gateway relay, not browsers dialling participant hubs.

## Fit against our requirements

| Requirement | MoQ fit |
|---|---|
| Conductor-pushed explicit forwarding | Data plane fits (a hub issues SUBSCRIBE upstream on instruction). No standard controller channel; IETF relay rules and moq-dev's automatic routing both pull the other way. |
| Make-before-break | Good: publisher-assigned group/object ids dedupe across two relays; switch at next group. |
| Hub crash recovery | Not addressed by IETF; moq-dev reroutes on withdrawal/timeout. Contingency routes stay our design. |
| Publish to several hubs | Supported. |
| Connected-not-full-mesh, multi-hop | IETF: out of scope. moq-dev: yes, but emergent and CDN-tuned. |
| Slots / last-N | No equivalent; MoQ is always subscriber-named tracks. With E2EE the track name is authenticated, so a hub cannot re-originate tracks to emulate slots. |
| Simulcast | Separate tracks + catalog. Fits. |
| Low-latency audio in datagrams | In the spec; lightly exercised in libraries. |
| E2EE with relay-rewritable metadata | Direct match (immutable/mutable properties + secure-objects). |

## What to adopt regardless

- Publisher-assigned monotonically increasing group ids and object ids. Dedupe key =
  (stream, group, object). **Not** MoQ's string track names: streams stay numeric internally (see
  Roadmap).
- One group per unidirectional stream; one datagram per audio object.
- Per-stream publisher priority and delivery timeout.
- `NEW_GROUP_REQUEST` semantics with hop-by-hop dedupe.
- `GOAWAY`-style drain signal with a timeout.
- Immutable vs mutable property split, compatible with secure-objects.
- Catalog (MSF/hang style) for renditions; LOC-style framing.

**Consequence for slots:** the earlier design had hubs rewrite a relay stream id to a subscriber's
slot id. The MoQ-shaped alternative is: every source is a named track; the plan (and hub-side
selection) tells each subscriber which tracks currently fill its slots; switching a slot is the hub
starting to deliver a different, already-authorised track at a group boundary. This keeps frame
authentication over the track name intact. To settle in the wire-protocol design.

## Roadmap: direct first, public MoQ relay network second (added after review)

Idea: nodes that can reach each other talk directly over our protocol; nodes that cannot go through
a public MoQ relay network (a CDN); the same network later carries distribution at scale.

**How the relay network appears in the design: a *passive hub*.** It is a hub with large capacity
and good reachability that the conductor cannot push a plan to. Its forwarding is determined
entirely by what members publish to it and subscribe from it — and the plan already tells members
what to send where and what to receive from where. So the topology computation treats it as one
more hub with a cost (latency, money, privacy) and no special path.

What it would solve:

| Problem | Via relay network |
|---|---|
| Node cannot reach any member | Publishes/subscribes its media and control through the network (TURN-like) |
| First contact / rendezvous | Members watch a namespace derived from `K_topic`; a joiner announces there. Works on UDP-hostile networks if the provider offers a TCP fallback |
| Meetings with no viable hub (all mobile/CGNAT) | The network is the hub |
| Large audiences | View-only participants subscribe from the network; the interactive core (≤50) stays on peer hubs, with one or two members publishing the "stage" streams to it |

What it requires:

1. **Frame encryption moves earlier.** The network is not a member, so media and control must be
   opaque to it before any of this ships (design §7 phase B).
2. **A MoQT *client* in `net`**, pinned to whatever drafts the providers deploy. A client is far
   smaller than a relay. Today that means draft-14/16 for Cloudflare, which the main Rust library
   (drafts 18+) does not speak — an immediate version problem, and it will recur until the RFC.
3. **A naming map at the edge only.** Internally streams stay numeric (sender id, stream number).
   Toward the network: namespace = (opaque hash from `K_topic`, sender id), name = stream number.
   No human-readable names, nothing meaningful to the provider.
4. **Control over the same path**: each node's reports and the plan travel as encrypted streams, so
   a relay-only node is still reachable by the conductor (the design already requires only a path).
5. **A provider abstraction**: public CDN, or an org's own `moq-relay`, behind one interface.

Costs and risks:

- It reintroduces a server dependency, so it must stay optional: a meeting whose members can reach
  each other never touches it.
- Provider sees IP addresses and traffic patterns, though not content.
- Audio latency if the provider has no datagram support (stream per frame).
- Hub-side behaviours that rely on reading our relay header (loudest-N, slot switching) do not
  exist on a passive hub: a subscriber behind the network gets what it subscribed to. Loudest-N for
  such a node must be done by a member hub publishing an already-selected set, or client-side.
- Free preview pricing will not last.

Order:

1. Direct only, numeric ids, MoQ-compatible group/object numbering (plan phases 1–3).
2. Frame encryption.
3. Relay network as rendezvous and link of last resort (passive hub).
4. Relay network for scale: view-only audiences, promotion between viewer and participant.

## Options and risks

| Option | For | Against |
|---|---|---|
| A. Use `moq-net`/`hang` wholesale | Fastest working media; browser/hang interop | Very high churn; dropped iroh; own QUIC + new crypto lib; routing is emergent, not conductor-explicit; seamless failover unfinished |
| B. IETF MoQT wire-compatible | Standards interop eventually | Moving target; relay must implement everything; relay-to-relay undefined. Revisit after RFC |
| **C. Own protocol, MoQ-shaped** | Fits conductor/slots/crash recovery/iroh; a MoQ gateway stays cheap | We own scheduling, priorities, congestion handling |
| D. `moq-net` as per-link pub/sub, conductor drives subscriptions, clustering disabled | Reuses a lot if it works | Carries A's dependency risk |

## Spike (about a week) to choose between C and D

1. Does current `moq-net` still run over iroh 1.x via an adapter, with `rind`/`moq-tokio` kept out
   of the dependency graph?
2. With transit/proxy off, can application code make "hub H subscribes to track T from U and serves
   it" happen with no automatic re-announcement?
3. Subscriber attached to two hubs for one track: switch at a group boundary with zero loss and no
   duplicates; then kill one hub and measure the gap.
4. Audio as datagram tracks vs stream-per-frame at 50 participants under 2–5% loss.
5. Last-N churn: cost of ~20 subscribe/unsubscribe per second; time to first frame.
6. Browser joining a native hub with a pinned certificate hash vs via a gateway.

If 1–3 pass cleanly, D is viable for a first version; otherwise C, with a MoQ gateway as a later,
separable component.

## Sources

datatracker.ietf.org/doc/draft-ietf-moq-transport (‑23) · datatracker.ietf.org/wg/moq/documents ·
github.com/moq-dev/moq (doc/concept/moq-lite.md, doc/bin/relay/cluster.md, PR #7345) ·
moq.dev/blog/update-01 · github.com/englishm/moq-interop-runner · blog.cloudflare.com/moq-relays ·
developers.cloudflare.com/moq · github.com/n0-computer/iroh-live
