# Spike 1 — transport

Naming: "relay" in this note always means **iroh-relay** (iroh's STUN/TURN-like connection
helper), never our MoQ relay (the **khef**).

Date: 2026-11-15. Code was throwaway (`spikes/`, git-ignored). iroh 1.4.1 (QUIC via `noq` 1.4.0),
quinn 0.11.12, Rust 1.95, WSL2.

## Questions and answers

| Question | Answer |
|---|---|
| Does iroh give us datagrams, per-group unidirectional streams, stream reset, and connection stats? | **Yes, all of them.** `send_datagram`/`read_datagram`, `open_uni`/`accept_uni`, `reset`, stream priority, `stats()` (packets/bytes sent and lost), per-path RTT, `congestion_state()`, and `export_keying_material()` (useful for binding the password handshake to the connection). |
| Does iroh work with no n0 infrastructure at all? | **Yes** for direct addresses: `RelayMode::Disabled`, `presets::Minimal`, dial by (endpoint id, socket address). |
| Can two NATed nodes connect with no n0 relay, using a member as introducer? | **Yes, two ways** (below). The simpler one needs no relay code at all. |
| What does plain quinn plus our own punching cost? | About 150 lines for the same result, with roughly a fifth of the dependencies. |

## NAT lab

Unprivileged network namespaces (`unshare -rnm`): a public segment with member M, and nodes A and B
each behind their own router doing nftables masquerade. "Cone" = port-preserving mapping with
port-restricted filtering (typical home router); "sym" = a different random port per destination;
"open" = no NAT.

| A – B | iroh, relay hosted by M | iroh, **no relay**, M introduces | quinn, M introduces |
|---|---|---|---|
| cone – cone | direct (after ~seconds; relay path kept as fallback) | **direct, ~10 ms** | direct, ~2 ms |
| open – cone | direct | — | direct |
| open – sym | — | **direct** | direct |
| cone – sym | relayed only | no direct path | no direct path |
| sym – sym | relayed only | — | no direct path |

- **"M introduces"** means: A and B each connect to M; M tells each the other's *observed* address
  (and, for iroh, endpoint id); both then dial each other at the same moment, and whichever
  connection completes first is used. This is the STUN-like function performed by a member, and
  it maps directly onto our design: observed addresses travel in reports on the control tracks
  and the plan tells both ends to peer.
- **Symmetric NAT on either side of a NATed pair defeats direct connection in every variant**, as
  expected. A symmetric-NAT node can still connect directly to any node with no NAT (the
  "open – sym" row). The design's answer stands: such a node attaches through a transit relay it
  can reach, and failing that through a TURN-like relay.
- **iroh's own relay can be embedded in a member** (`iroh-relay` with the `server` feature) and
  does coordinate hole punching, but it is a poor fit: it is an HTTPS/WebSocket service that
  expects a CA-trusted certificate (the skip-verification switch is test-only), it pulls in a
  large server stack (ACME, HTTP), its fallback path is TCP-based and unsuitable for media, and
  it needs a second QUIC endpoint for address discovery before any punching happens. Not
  recommended.

## Other observations

- **Datagram size**: 1162 bytes usable on the IPv6 loopback path. Keep audio objects plus headers
  well under ~1100 bytes.
- **Dependency weight**: the iroh spike resolves roughly four to five times as many crates as the
  quinn spike (HTTP client, DNS resolver, and relay client come along even with relays disabled),
  and the debug binary was 248 MB against 53 MB.
- **Lab lesson for the emulated-network test tier**: a Linux router that lets unsolicited inbound
  packets reach its own input path records them in connection tracking, which then forces a
  *different* source port on the next outbound packet and silently breaks hole punching. The lab
  router must drop new inbound packets on the WAN side, as real routers do.
- Build times here: 21 s cold for the iroh spike, 15 s for quinn.

## Recommendation

**Superseded 2026-11-20: quinn, with no relay.** When iroh was added to the product it
took the dependency tree from 65 packages to 379 and needed licence and advisory
exceptions; quinn takes it to 112 with none. See design sections 3 (Reachability) and 12
(Transport). What follows is the earlier reasoning, kept for the record.

**Revised 2026-11-16: use iroh with a public or self-hosted iroh-relay enabled by default (URL
list configurable), and do introduction ourselves over the control tracks when a reachable
member exists.** Keep it behind the transport trait with quinn as the fallback. Not embedding an
iroh-relay in members still stands.

Why the iroh-relay stays on: member introduction needs a publicly reachable member, which a
call between two NATed participants does not have; and it gives symmetric-NAT pairs a path
when no transit khef is reachable.

Connection outcomes (from the 1.4.1 source): a connection is always QUIC end to end, over a
direct UDP path, an iroh-relay path (WebSocket over TLS over TCP to the iroh-relay), or both
at once with the relayed path as standby. There is no direct TCP and no UDP relaying. Media
preference: direct, then via a transit khef, then the iroh-relay path.

Why iroh rather than quinn, given that we do the punching coordination ourselves either way:

- Connections are dialled and mutually authenticated by public key out of the box, which matches
  our per-meeting node identity.
- Multipath and path selection are built in (seen working: relay path → direct path with the old
  path kept), which is what network changes on laptops and phones need.
- Port mapping (UPnP/PCP/NAT-PMP) and address discovery are included.
- Maintained mobile bindings and NAT-traversal logic we would otherwise own.

What it costs: the dependency weight above, coupling to n0's QUIC fork, and API churn risk.

Not verified in this spike, to check when the `net` crate is built:

- Port mapping and multipath behaviour with relays disabled.
- Usage limits on n0's public iroh-relays, and the cost of self-hosting one.
- Whether a dial with known direct addresses tries them alongside the iroh-relay path, and
  whether the iroh-relay can be enabled lazily (nodes otherwise hold an idle connection to it
  for the whole meeting, exposing IP address and endpoint id). On by default until checked.
- Behaviour on a network change (interface switch) without a relay path to fall back on.
- IPv6.
- How much of the dependency weight can be removed with feature flags.

If any of those disappoint, quinn is a small step: identity-pinned TLS (about a hundred lines)
plus the standalone `portmapper` crate.
