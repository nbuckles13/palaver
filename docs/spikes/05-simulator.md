# Spike 6 — simulator choice

Date: 2026-11-16. Throwaway code (`spikes/simcmp`; timing added to `spikes/topn`).

## Question

Which deterministic harness does the `sim` crate use: an existing framework, or our own?

## What was compared

| Candidate | What it is | Finding |
|---|---|---|
| **Our own discrete-event loop** over the sans-IO cores | A virtual clock, one seeded random source, an ordered event queue, and a link model. Spikes 2–4 were already written this way | A 600 s, 50-node meeting with every audio packet (1.8 million link deliveries, top-N selection at every hop) ran in **1.4 s**; a 60 s meeting is about 140 ms |
| **turmoil** 0.7.2 (tokio-rs, MIT) | Runs real async code for many hosts in one thread, with simulated sockets, latency, loss, partitions and a seed | Deterministic (same seed, identical arrival order; different seed differs). 50 hosts at audio packet rate: 60 s simulated took **11.3 s** at its default 1 ms tick, 2.3 s at a 5 ms tick. Only 57% of packets arrived with 2% loss set; not investigated |
| madsim 0.2.34 | Replaces the tokio runtime with a deterministic one | Not tried: needs our code and dependencies built against its runtime; last release October 2025 |
| shuttle, loom | Explore thread interleavings | Not relevant yet: the cores are single-threaded state machines |

## Decision proposed: build our own, small

The design already makes the logic sans-IO, so a simulator does not need to intercept sockets,
timers or tasks, which is the hard problem turmoil and madsim solve. What is left is a few
hundred lines, and it is 50–100 times faster for our workload, which is what keeps simulation
tests inside the one-second budget.

turmoil could later test the async shell (the driver around the cores), but our transport
(iroh, or quinn) does not run on its sockets, so it would cover little that the in-memory
transport plus tokio's paused clock does not.

## Shape of `sim`

- **World**: virtual clock in microseconds; one seed, with separate random streams derived per
  node and per link so that adding a node does not shift every other draw; an event queue
  ordered by (time, sequence number) so ties are broken the same way every run.
- **Nodes**: each holds the real sans-IO cores (`khef-core`, `control`, `topology`, and a model
  client), driven only through "handle this input at time t" and "when do you next need
  waking".
- **Links**: per direction: delay distribution, jitter, loss, reordering, bandwidth cap with a
  queue, up/down. A reachability matrix says which pairs can connect at all (NAT classes,
  blocked pairs), so non-full-mesh graphs are the default case.
- **Faults**: crash, restart, graceful leave, partition and heal, slow node, link degradation,
  all schedulable at a virtual time.
- **Scenarios**: written by hand for known cases; generated from a seed for exploration. A
  failing seed is printed and replays exactly (`SIM_SEED=N`). `proptest` supplies generation
  and shrinking for the generated ones.
- **Invariants**, checked as the run proceeds: no forwarding loops; at most N audio tracks per
  overlay link; no duplicate delivery to a client; group ids never go backwards; one conductor
  per term; plan generations only increase. And eventual ones: everyone hears a new speaker
  within a bound; the meeting reconverges after a fault.
- **Trace**: a structured event log, identical for identical seeds, kept on failure.

## Determinism rules this needs (guards)

- No wall clock, threads, or ambient randomness in sans-IO crates (already a layering guard).
- **No iteration over hash maps with random order** in sans-IO crates: use ordered maps or a
  fixed hasher. Enforced with clippy's disallowed-types.
- A self-test that runs one scenario twice with the same seed and requires identical traces,
  and once with another seed and requires a different one. This is the guard that the
  simulator itself is deterministic.

## Not covered

- The async shell: tested with the in-memory transport and tokio's paused clock, and by e2e.
- Whether scenarios can be shared between `sim` and `e2e` (same script, simulated or real
  processes). Worth trying in phase 1.
