# Palaver

Multi-party audio/video meetings with no operated servers. Every client embeds a **khef**, a small
Media over QUIC (MoQ) relay of our own; khefs connect to each other, and one elected client (the **conductor**) decides how
they are wired and how media is routed between them, based on resources, connectivity and
latency. Those roles can move without interrupting media.

Why: lower cost, more privacy, no dependency on servers being up, minimal IT roll-out.

Status: design exploration. Open source (licence to be chosen; permissive).

## Documents

1. [Research](docs/01-research.md) — prior art, building blocks, NAT reality, cross-platform stack.
2. [Design](docs/02-design.md) — the current design: every node runs a small MoQ khef; clients
   subscribe to named tracks; the conductor publishes routing tables and policy.
3. [Implementation plan](docs/03-plan.md) — spikes, phases and demos.
4. [Development environment and flow](docs/04-dev-environment-and-flow.md) — test tiers, guards,
   time budgets, and a lightweight autonomous dev flow.

`docs/archive/` holds background: the two earlier design drafts merged into document 2, and the
Media over QUIC evaluation that led to the current design.

## Stack (summary)

- **Core**: one Rust workspace exposing an SDK; sans-IO decision logic; deterministic simulator;
  headless bot as the first consumer.
- **Transport**: QUIC via iroh 1.0 (quinn as fallback); Media over QUIC Transport on every link,
  with our own khef implementing a small profile of one pinned draft.
- **Media**: libwebrtc where it is the best tool (audio processing first), platform codecs, Opus.
- **UI**: egui harness for development; product shell (Flutter or Slint) deferred.
