# Research: prior art and cross-platform stack

Date: 2026-11-14. Items marked **[unverified]** come from memory or weak sources and were not
confirmed by search. Source URLs are at the end.

## 1. Prior art

**Headline: nothing found that does this.** No shipping product or open-source project combines
client-embedded forwarders, resource/latency-based election, make-before-break migration and crash
repair. The pieces exist separately.

### Closest analogs

| Project | What it does | Where it falls short for us |
|---|---|---|
| **Jami** (GPLv3, active) | OpenDHT discovery; the conference *host device* mixes audio/video for everyone (an MCU on a client). | Single host, no election, no migration (docs list host hand-off as future work), mixing CPU cost, account-based. |
| **Keet / Holepunch** (active; app closed, stack open) | HyperDHT = Kademlia with built-in hole punching; topic = hash(shared secret); opt-in peer "blind relay" when both NATs are hostile. | JS runtime, group media topology undocumented, no visible forwarder election. Best evidence that DHT + peer relay works at consumer scale. |
| **Classic Skype** (dead as P2P since 2012) | Client supernodes promoted on reachability, uptime and bandwidth; relayed signalling and media. | Relays, not SFUs; central login. Lessons: load is very skewed toward a few supernodes; mobiles make poor supernodes; correlated supernode failure is an outage **[2010 outage cause unverified]**. |
| **iroh-live** (n0, MIT/Apache, "early tech preview") | Rust, MoQ over iroh QUIC, hardware H.264, simulcast, gossip rooms, wgpu/egui renderer, synthetic test sources. | No topology/election logic; relies on n0 relays/DNS by default; Windows "builds in CI, never run"; iOS never built; depends on a patched moq fork. Closest *code* to what we need. |
| **MatrixRTC / Element Call** | Per-participant "focus" (SFU) membership; multi-SFU landing mid-2026, where each participant publishes to its own SFU and subscribes from others'. | Server-operated LiveKit SFUs. The focus model maps directly to "each hub serves its own clients". |
| **Jitsi P2P mode** | 2-party P2P with the bridge connection kept warm; third joiner switches to the bridge. | Server-based. Small, proven make-before-break pattern. |
| **Trystero** (JS) | WebRTC full mesh; signalling over Nostr, BitTorrent trackers, MQTT. | Mesh only. Proves public Nostr relays / BT infrastructure work as zero-ops rendezvous. |
| **SnoW** (arXiv 2206.12762) | Browser-only mesh/SFU/MCU equivalents without media servers. | Research prototype; no dynamic election or repair visible. |

Not applicable: Tox (group video never matured), Briar/Quiet (Tor latency, no calls), Berty,
RetroShare, PeerTube-style P2P (seconds of latency), Galene / Peer Calls (server SFUs; useful as
small readable SFU references).

**Patents to check for freedom-to-operate:** Topia "mixed peer-to-peer mesh and forwarding system"
(US 11,758,003; 12,261,914); legacy Skype/Microsoft supernode patents (not searched).

### Academic work worth stealing from

- **Narada / End System Multicast**: keep a richer *control mesh* and derive per-source delivery
  trees from it; latency-weighted; aimed at small conferencing groups.
- **NetALM, biconnected overlays**: reactive orphan-rejoin is too slow for real-time; **precompute a
  backup parent** and place high-capacity nodes near the root.
- **Celerity** (ACM MM 2011): fully P2P conferencing with delay-bounded tree packing and rate
  adaptation.
- **Super-peer selection criteria** (Skype measurement studies): public reachability, uplink
  capacity, session age, RTT centrality.

### Building blocks

| Block | State (Nov 2026) | Use for us |
|---|---|---|
| **iroh 1.0** (June 2026) | Stable wire/API, QUIC (via `noq`, n0's quinn fork with multipath), dial-by-public-key, hole punching, Swift/Kotlin bindings. Defaults depend on n0's DNS/pkarr and relay servers; mainline-DHT and mDNS discovery are opt-in crates. **Relay-less hole-punch coordination is not available out of the box** (sources conflict). Datagram API in 1.0 **[unverified]**. | Leading transport candidate. The relay dependence is the #1 thing to spike. |
| **quinn 0.11** | Mature, no multipath; used by dark_tower. | Fallback transport; we would own NAT traversal. |
| **iroh-gossip** | HyParView + PlumTree; needs bootstrap peers. | Possible membership substrate; probably unnecessary at our sizes (see design). |
| **distributed-topic-tracker** | Third-party crate; bootstraps iroh-gossip topics from the mainline DHT with encrypted records, no servers. Small project. | Essentially "meeting id + password → peers". Audit or reimplement. |
| **Mainline DHT** (BEP5 / BEP44 / pkarr) | Largest, most durable public DHT. ~1 kB values, exposes IPs, UDP only, sybil/eclipse risks. | Primary internet rendezvous. |
| **Nostr relays** | Volunteer WSS relays; ephemeral events as a mailbox. | Secondary rendezvous that works on UDP-blocked networks. |
| **MoQ** (`moq-dev/moq`, IETF MoQT draft-23) | Very active but still breaking between drafts; moq-relay dropped its iroh backend on 2026-11-03. Model: named tracks, group-per-stream, chainable relays. | Same shape as ADR-0036 (GoP per QUIC stream). Borrow the model; don't depend on the moving draft. |
| **str0m** | Sans-IO WebRTC, production SFUs. | Only if browser WebRTC interop becomes a requirement. |
| **webrtc-rs 0.20** | Rewrite; no BWE, pacer or jitter buffer yet. | No. |
| **libp2p (rust)** | Any peer can be a circuit relay (good fit), but slow release cadence, heavier API, no media story. | Not recommended. |
| **PAKE** | CPace (CFRG draft, late stage) or SPAKE2 (RFC 9382, `spake2` crate **[unverified]**). OPAQUE is client-server and does not fit. | Password authentication on every new peer connection. |
| **MLS / openmls** | No 1.0 confirmed; needs a total order on commits (a server, or a designated committer). | Overkill at our sizes; ADR-0036 already rejected MLS on coordination grounds. |
| **SFrame** (RFC 9605) | Standard per-frame E2EE through forwarders. | Already the basis of ADR-0036 §4. |

### NAT traversal without TURN

- Best data: libp2p DCUtR measurement (arXiv 2604.12484): **~70% ± 7% hole-punch success** among
  NATed pairs; nearly all successes on the first try. Vendor figures are rosier (Tailscale ">90%"
  direct with port mapping/IPv6; n0 "~95% of bytes direct").
- Planning number: **10–30% of NATed pairs fail direct.** Symmetric↔symmetric (CGNAT, enterprise) is
  the hard case. UDP-blocked corporate networks cannot be fixed by any UDP peer relay.
- **Why a hub topology helps:** a full mesh needs every pair to connect; a hub topology needs every
  client to reach *some* hub. A symmetric-NAT client can usually reach a hub with a public or
  endpoint-independent mapping. Residual failures: meetings where nobody is well connected (all
  mobile), UDP-blocked participants, and the first-contact bootstrap (hole punching needs a
  coordination path before any hub is known).

## 2. Cross-platform stack

### UI layer

| Option | Platforms | Many video tiles | Verdict |
|---|---|---|---|
| **Flutter + flutter_rust_bridge** | All five, mature | External textures (`irondash_texture`); production precedent (LiveKit Flutter SDK, RustDesk) | **Best product shell.** Adds Dart; 2–4 weeks of texture/bridge plumbing. |
| **Slint** | Desktop solid; mobile improving (1.15–1.17) | Imports wgpu textures directly, one path everywhere | **Best all-Rust option.** Smaller widget ecosystem; mobile accessibility unknown; GPL / royalty-free-with-attribution / commercial licence. |
| **egui** | Desktop good, mobile rough | wgpu textures directly; iroh-live ships such a widget | **Demo/debug harness**, kept permanently. |
| **Tauri 2** | Desktop mature, mobile thinner | Weak: native frames cannot be composited zero-copy into a webview; WebKitGTK is the weak link | Tempting shortcut, likeliest rewrite. |
| GPUI | No mobile | Proven in Zed | No. |
| Dioxus / iced / Makepad / Xilem | Immature mobile and accessibility; Dioxus team acqui-hired Sept 2026 | — | No. |
| Compose Multiplatform + UniFFI | Desktop is JVM | Texture interop awkward | Only for a Kotlin team. |
| React Native + UniFFI | No Linux **[unverified]** | Native view per tile | Fails Linux requirement. |
| Qt/QML + cxx-qt | All five, very mature | Excellent | Credible but heavy; LGPL/commercial; C++ glue. |
| Electron + Rust | No mobile | Best web media stack | Violates shared-code goal. |

### Media pipeline

| Option | Fit | Risk |
|---|---|---|
| **(a) libwebrtc via LiveKit crates** | Best AEC/NS/AGC, and the APM is usable standalone. Everything else (NetEQ, codec factories, simulcast) lives inside PeerConnection, which means ICE/SRTP rather than our QUIC forwarder. | Low quality risk, high architecture mismatch. |
| **(b) Rust pipeline behind traits** | `cpal` + Opus; platform voice processing on Apple/Android, `sonora` (young pure-Rust AEC3 port) or libwebrtc APM elsewhere; platform hardware H.264 with a software fallback; `nokhwa`/`scap` capture. Every frame is ours to forward. | **We write the jitter buffer.** Audio quality is the main danger. |
| (b′) Reuse iroh-live's `moq-media` | Same shape as our design. | Preview quality; Windows unproven. Use as reference or starting fork, not a dependency. |
| (c) GStreamer | Broadest codec coverage. | Pipeline model fights a custom forwarder; mobile packaging painful; LGPL. |
| (d) Webview WebCodecs | Fast demo. | Splits the pipeline across JS and Rust; dies on mobile backgrounding. |

Hard problems regardless of choice:

- **Echo cancellation**: behind a trait; platform units where they exist.
- **Jitter buffer**: no standalone NetEQ-equivalent crate. Adaptive playout + Opus PLC/FEC first.
- **Simulcast**: hardware encoders give temporal layers at best; do simulcast as 2–3 encoder
  sessions and keep the forwarder codec-agnostic (it already is under ADR-0036 §7).
- **Congestion control**: QUIC datagrams are governed by NewReno/Cubic/BBR, not a delay-based
  real-time controller. Derive encoder bitrate from connection stats and drop stale data.

### WSL2 development (measured on this machine)

- WSL 2.6.1, kernel 6.6.87.2, WSLg present, `networkingMode=mirrored` already set.
- No `/dev/video*` by default; `uvcvideo` is in the kernel, so a USB camera via `usbipd` should work
  (not tried). No `v4l2loopback`/`vivid`/`snd-aloop`, so virtual devices need a custom kernel.
- WSLg mic works via the RDP source but with latency that makes it useless for AEC tuning.
- Conclusions: **synthetic sources live in the engine** (test pattern/Y4M, sine/WAV, null sink);
  software codecs in WSL; NAT traversal tested with network namespaces plus real Windows/macOS
  builds; real-device media validated natively.

### Mobile constraints that shape the design now

- **Hosting a hub is a role, not a property of every node.** Phones are last-resort hubs and must be
  able to resign (thermal, battery, backgrounding) → hub migration is needed from day one anyway.
- iOS: process survives in background only with an active audio session; no listening while
  suspended; local-network permission prompt for LAN peers.
- Carrier-grade NAT often defeats hole punching; prefer IPv6.
- Licences: avoid GPL and statically linked LGPL; prefer platform H.264 encoders (patents).
- Engine API must be FFI-friendly: no UI types, async events out, texture handles for video.

## Sources

Prior art: Jami docs (conference-protocol, calls-in-swarm) · docs.pears.com (HyperDHT) ·
arxiv.org/pdf/cs/0412017 (Skype) · matrix.org TWIM 2026-06-19 · jitsi.org/blog/p2p4121 ·
github.com/dmotz/trystero · arxiv.org/abs/2206.12762 (SnoW) · arxiv.org/abs/1107.1138 (Celerity) ·
arxiv.org/pdf/1310.4291 · NetALM (Springer 10.1007/978-3-642-22543-7_12)

Building blocks: iroh.computer/blog/v1 · iroh.computer/blog/noq-announcement ·
docs.iroh.computer/concepts/discovery · github.com/n0-computer/iroh-live ·
docs.rs/distributed-topic-tracker · datatracker.ietf.org/doc/html/draft-ietf-moq-transport-23 ·
github.com/moq-dev/moq (PR #7345) · docs.rs/str0m · webrtc.rs/blog/2026/08/01/the-path-to-webrtc-1.0 ·
datatracker.ietf.org/doc/draft-irtf-cfrg-cpace · github.com/openmls/openmls

NAT: arxiv.org/abs/2604.12484 · probelab.io/blog/can-libp2p-punch-through-nats ·
tailscale.com/blog/nat-traversal-improvements-pt-1

Stack: docs.rs/crate/irondash_texture · slint.dev/blog (1.15–1.17) · v2.tauri.app/blog/tauri-2.12 ·
docs.rs/crate/sonora/0.1.0 · github.com/tonarino/webrtc-audio-processing · docs.rs/crate/libwebrtc ·
docs.rs/crate/cpal · docs.rs/crate/nokhwa · learn.microsoft.com/windows/wsl/networking ·
github.com/dorssel/usbipd-win/wiki/WSL-support
