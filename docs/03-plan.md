# Palaver — rough implementation plan

Draft 3 (MoQ-based design). Goal: something people can use as early as possible, without
throwaway code. **The final crate boundaries, the khef protocol and the plan format exist from
phase 1**; each phase replaces a trivial implementation behind a boundary with a real one.

| Boundary | First implementation | Later |
|---|---|---|
| Rendezvous | Ticket (invite link with addresses) | mDNS, mainline DHT, TURN-like fallback |
| Admission | Shared secret in ticket | PAKE on password |
| Topology | Simple rule (every capable node is a transit khef; peer with the nearest two) emitted as a real plan | Scored, capacity-aware plans |
| Conductor | Meeting creator; no succession | Hand-over, succession |
| Routing | Default per-participant routes only | Per-track entries, caps, contingencies |
| Rate control | Fixed bitrates, one rendition | Simulcast, shedding, marking, adaptation |
| Media security | QUIC/TLS per hop | Frame encryption |
| Codecs | Opus + software H.264, then hardware H.264 on Windows (phase 2) | Hardware H.264 on the other platforms |
| UI | egui harness | Product shell |

Development flow and test tiers: [04-dev-environment-and-flow.md](04-dev-environment-and-flow.md).
Spikes are local and thrown away; only their written conclusions are kept (`docs/spikes/`).

## Phase 0 — spikes

Status: 1–6 done (see `docs/spikes/01-transport.md`, `02-moq-relay.md`, `03-topn-audio.md`,
`04-media.md`, `05-simulator.md`).

1. **Transport**: does iroh 1.0 give datagrams, per-group uni streams and usable connection
   stats? Can two NATed nodes connect with no n0 relay, using a member as introducer? Cost of
   quinn + own punching instead?
2. **MoQ profile**: minimal khef over our transport trait — two khefs, an in-memory client
   each, one video track subscribed by name end to end. Can MOQtail or `moq-net` serve as wire
   codec only? Which draft do we pin?
3. **Khef chain**: three khefs in a line (A–B–C); hop-by-hop subscribe from a routing table;
   re-route with dedupe (target zero loss); crash recovery time.
4. **Top-N audio**: 50 simulated publishers over a khef tree; convergence, per-link count ≤ N,
   speaker-change latency; datagrams vs stream-per-object under 2–5% loss.
5. **Media parts**: originally "libwebrtc à la carte". Outcome: no libwebrtc; `sonora`, libopus
   1.6, `cpal`, own jitter buffer, own video interface (Linux and Windows tested).
6. **Simulator**: choose or build the deterministic harness. Outcome: build our own.

Exit: transport, draft and codec approach chosen; workspace skeleton; CI on Linux/Windows/macOS.

## Phase 1 — audio through khefs, headless first

- `moq-wire` (our own draft 22 codec for the profile, cross-checked against MOQtail /
  `moqtap-codec` in tests), `proto`, `khef-core`, `net` (QUIC + in-memory), `control` (reports and plan as
  tracks, flood rule), `sdk`, `bot`, `guards`.
- Ticket join; creator is conductor; real plan format.
- Audio: capture → processing → Opus → datagram objects → khefs → jitter buffer → playout;
  top-N in its simplest form; synthetic sources.
- Simulator tests from day one, including a three-node line with no A–C link.
- Minimal egui app on the SDK.

**Demo 1:** a voice call between a few machines from a pasted link, with bots filling the room.

## Phase 2 — video, multi-khef

- Video: capture, encode behind our own interface (openh264 software first, then Media
  Foundation hardware on Windows in this phase), group per stream, subscribe by name, keyframe
  requests, tiles.
- Default routes, hop-by-hop resolution, dedupe; every edge khef peered with ≥2 khefs.
- Roster, self mute, host role, catalog track.
- Stats overlay; diagnostics bundle; local multi-bot end-to-end tests.

**Demo 2 (first user-usable build):** 4–8 people with audio and video on Windows/macOS/Linux,
joining by link, media crossing more than one khef.

## Phase 3 — the thesis: khefs and conductor move

- Reports with health and link metrics; scored, capacity-aware plans; hysteresis.
- Graceful transit-khef leave/resignation, make-before-break; simulator invariant "zero lost
  objects" and a real-network audio-continuity test.
- Contingency next-hops and crash recovery with a measured bound; decide whether audio needs a second overlay tree.
- Conductor hand-over within the 3 s cap; succession on crash.
- **Rate-control spike** (end of phase): throttle the middle link of a khef chain and the
  publisher uplink; compare receiver-side switching with khef-side track swap; decide the
  design for phase 4.

**Demo 3 (viability):** whoever carries the media — and the conductor — quits and nobody notices;
a machine is killed and the meeting recovers in under a second.

## Phase 4 — real discovery, rate control, scale to 25–50

- Meeting id + password: Argon2id/HKDF, PAKE admission; mDNS and DHT rendezvous;
  member-as-introducer punching; iroh-relay fallback; headless node as a reachable khef.
- Simulcast; priority shedding, congestion marking, rendition switching per the spike; uplink
  adaptation; caps and per-track routes; hardware H.264 on the remaining platforms.
- Control hardening: plan deltas, convergence at 50 nodes.
- Emulated-network matrix; nightly soak; distributed bot runs (cloud VM added here).
- Frame encryption (prerequisite for any non-member relay), then optionally a public MoQ relay
  network for rendezvous and unreachable nodes.

**Demo 4:** 25–50 participants (humans plus bots) across home networks with only a meeting id and
password, surviving khef and conductor churn.

## Phase 5 — product

- Product UI shell (Flutter or Slint), installers, auto-update, accessibility.
- Moderation: remote mute enforced at the publisher's edge khef and upstream, kick, lock.
- Screen sharing, chat/data tracks, whiteboard.
- SDK bindings (Python first); reference recorder and transcriber participants.
- Thin web client, if wanted.

## Phase 6 — mobile

- iOS/Android shells over the same SDK; platform voice processing; background rules.
- Phones run an edge khef only, or attach as thin clients; transit duty as a last resort.

## Sequencing notes

- Headless before GUI in every phase.
- Phase 3 holds the novel risk, so it precedes real discovery; if the transport spike shows
  first-contact NAT traversal is bad, pull discovery forward.
- Windows and macOS build in CI from phase 0; real-device media is validated on the Windows host
  and the MacBook.
