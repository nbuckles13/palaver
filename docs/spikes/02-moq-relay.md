# Spike 2/3 — MoQ profile, wire codec, khef chain

Naming: **khef** is our MoQ relay, one per node.

Date: 2026-11-15. Throwaway code (`spikes/moq`, about 400 lines). MOQtail 0.18.0 as codec; khefs
connected by in-memory links shaped like QUIC (request streams, unidirectional streams). No real
network in this spike, so timings are logic-only.

## Wire codec: is there a usable crate?

| Candidate | Finding |
|---|---|
| **MOQtail** 0.18.0 (Apache-2.0, "draft 22") | **Usable.** Its `model` module (~16k lines including tests) is a pure byte codec with no IO: control messages, subgroup headers and objects, datagrams, properties, range filters, `SUBSCRIBE_TRACKS`, `REQUEST_UPDATE`, `GOAWAY`. Payloads are forwarded without copying (`serialize_parts`), and parsing reports "not enough bytes" so it can be driven from a stream. Catch: the crate has no feature flags and always pulls in its own transport (`wtransport`, full `tokio`), which we would not use. |
| **moq-net** 0.3.23 | **Not separable.** ~100k lines; the IETF encoding is interleaved with its own session, cache and routing model. |

**Decided (2026-11-16):** pin **draft 22** and use MOQtail's `model` through our `moq-wire` crate,
so nothing else in the workspace sees it. Depend on the `moqtail` crate directly: the client
needs tokio anyway and the unused `wtransport` dependency is accepted. The crate's `relay` and
`client` modules are empty; `model` has one reference into `transport` (an enum in setup). Not assessed: what
changed between drafts 22 and 23.

## Minimal khef: what was built

- `SUBSCRIBE` / `SUBSCRIBE_OK` on a per-request bidirectional stream (draft 22 puts every request
  on its own stream; cancelling a subscription is closing that stream).
- Objects on one unidirectional stream per group, khefs rewriting only the track alias.
- Routing table with longest-prefix match and an ordered next-hop list (primary, then
  contingency); hop-by-hop upstream subscribe; publisher produces only while subscribed.
- Dedupe by (track, group, object) when a track arrives from two upstreams.
- Topology: publisher — A — {B, B2} — C — subscriber.

## Results

| Test | Result |
|---|---|
| Subscribe by name across A–B–C | Works; first object ~1 ms after subscribe when it lands on a group start |
| Steady state | No missing objects, no duplicates |
| **Planned re-route** (C switches B → B2, make before break) | **Zero missing, zero duplicates delivered**, every run. C dropped 1–11 duplicate objects internally during the overlap, then released the old upstream |
| **Crash of the active upstream** (C falls back to its contingency hop) | Loses the remainder of the interrupted group only; delivery resumes at the next group start. Gap ≤ one group duration (50 ms here), excluding detection time, which only a real transport can measure |
| Unsubscribe | Propagates hop by hop; khefs drop the track; the publisher stops producing |

## Design rules this surfaced

1. **Group ids must keep counting for the life of a track**, across unsubscribe and resubscribe.
   A publisher that restarts numbering breaks dedupe downstream (seen: 80 duplicate deliveries
   until fixed). Same rule as dark_tower's GoP counter.
2. **Objects can arrive before the subscribe reply.** The data stream and the reply race; a khef
   that drops a stream whose alias it does not know yet loses a whole group (seen: first object
   after 51 ms instead of 1 ms). Streams with an unknown alias must be held briefly.
3. **Linger before unsubscribing upstream.** When a khef's last downstream subscriber vanishes
   during a failover, an immediate upstream unsubscribe ripples to the publisher, which stops and
   restarts. A short hold (on the order of a second) avoids that churn.
4. **Make-before-break rule that worked:** keep both upstreams until the new one delivers a group
   start, then release the old one.
5. **A new subscriber waits for a group start.** Without a keyframe request that is up to one
   group duration, so `NEW_GROUP_REQUEST` is needed for real video.
6. **Dedupe state must be bounded**: a small window of recent groups per track.
7. The transport abstraction needs exactly three things: bidirectional request streams,
   unidirectional streams, datagrams.

## Not covered (next)

- Real QUIC under the khef (stream framing, setup exchange, the iroh adapter) and therefore
  crash *detection* time.
- Top-N audio and datagram objects (spike 4).
- Interop probe against another MoQ implementation.
- Draft 22 → 23 differences.
