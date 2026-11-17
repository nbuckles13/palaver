# Spike 4 — top-N audio over the overlay

Date: 2026-11-16. Throwaway code (`spikes/topn`, about 250 lines, std only; `spikes/moq/src/bin/wire.rs`
for the wire check). Logic-only simulation on a fake clock: one tick = 20 ms = one packet
interval = one hop. Timings are therefore in hops, not real milliseconds.

## Setup

- 50 nodes, N = 3. Eight transit khefs (each with its own client) and 42 leaves. Two overlay
  shapes: a tree (longest path 7 hops) and a chain of the eight transit khefs (9 hops).
- Publishers always send: a packet every 20 ms while speaking, three trailing packets, then a
  DTX packet every 400 ms. Speech levels vary per speaker and per packet, with 12% of speech
  packets dipping below the minimum level (gaps between syllables). The first packet of a word
  is 15 below the speaker's normal level.
- The selection rule is the one in the design (section 8): per khef, per overlay link and for
  the local client; ranked by level on packet arrival; a newcomer above the minimum level
  replaces the quietest holder that has not ranked in for the hold time; no timers.

## Results

| Scenario | Result |
|---|---|
| One-word answer (300 ms) from a participant outside the top N, holders silent 5 s | **All 15 packets reach all 49 listeners**, both shapes, hold 100 / 250 / 500 ms. The farthest listener gets the first packet after exactly the hop count (7 or 9): selection adds no delay |
| Answer 200 ms after a 3 s question | All packets delivered, every hold |
| Fourth person answers during a rapid three-way exchange (200 ms turns) | Hold 100 or 250 ms: complete. **Hold 500 ms: first 180 ms lost** (all three holders still protected) |
| Same with 400 ms turns | Complete at every hold |
| 120 s simulated meeting (61 talk spurts, overlaps, short interjections) | Hold 100 / 250: no onset clipped, 100% of speech packets to all listeners. Hold 500: one onset clipped by 120 ms |
| Same with 5% loss per hop and jitter | Deliveries 82.4–83.0% against 82.8% from loss alone: no measurable cost from the rule |
| Link load in that meeting | Mean 30, max 58 packets/s per link, against 88 and 173 with no filtering |
| Six people talking at once | Selection changes 0.3–0.6 times per second per listener; listeners end with different subsets, as accepted |
| First frame of a word below the minimum level | That one packet (20 ms) is lost; the rest arrive |
| A selected talker crashes mid-speech | Without level decay its last level protects its slot for ever and a later speaker gets 5 of 50 packets. With the rule (below) all later speakers are complete |

## What this adds to the design

1. **Hold time: 250 ms.** 100 and 250 behaved the same without contention; 500 clipped a quick
   answer in fast conversation. 250 halves the switching rate of 100 when many talk at once.
2. **Level decay (new).** A track's level is held for two packet intervals after its latest
   packet, then decays linearly to zero at 100 ms. Evaluated on packet arrival, so still no
   timers. Needed for crashes and for lost end-of-speech packets. (First run as a hard 100 ms
   cut-off; re-run with the decay, results unchanged except where noted below.)
3. **Soft onsets.** A word whose first frame is below the minimum level loses that frame. Options
   if this is audible: a lower minimum level, or each khef keeps the latest packet of every
   unselected track and sends it ahead of the first forwarded one. To be judged with real audio
   in phase 1.
4. **Free slots at start-up** are filled by the first tracks seen, speech or not.
5. **Entry is checked only for the arriving track.** A track blocked by the hold is retried on
   its next packet 20 ms later, which is enough.

## Wire check (MOQtail 0.18.0)

| Question | Answer |
|---|---|
| `audio_level` as an object property on a datagram | **Works.** Unknown object properties are preserved and round-trip |
| Audio datagram overhead | **10 bytes** over the Opus payload (alias, group, object, priority, one property). 80 B → 90 B; a 400 B worst case → 410 B. Far under the ~1100 B limit, with room for FEC and DRED |
| A top-N parameter on `SUBSCRIBE_TRACKS` | **Not possible.** Message parameters are a closed enum; an unknown parameter type is rejected as a protocol violation on parse, and there is no variant to build one |

**This is the draft's rule, not MOQtail's opinion.** Draft 22 section 9.20: every message
parameter must be defined by the negotiated version or negotiated through a Setup Option; an
unknown one closes the session, and the parameter block has no way to skip one. A standard
extension is therefore a Setup Option plus a parameter whose encoding both sides know, and a
codec needs a hook for registering such parameters.

Other Rust crates checked (crates.io, 2026-11-16):

| Crate | Finding |
|---|---|
| `moqtap-codec` 0.13.0 (MIT) | Pure codec, depends only on `bytes` and `thiserror`; drafts 7–23 behind one feature flag each (draft 22 is about 5.7k lines); object properties kept as opaque bytes; reports short reads. Rejects unknown message parameters too, with no registration hook (the table is a private function). First published April 2026, 14 releases |
| `moq-transport` 0.17.1 (Cloudflare `moq-rs`) | Draft 18; codec bundled with its session and tokio; not separable |
| `moq-net` | Already ruled out (spike 2) |
| `moq-core` 0.0.1, `moqt` 0.0.1 | Draft 16 or older; inactive |

No crate offers an extension hook. Options: (a) our own codec in `moq-wire` for our profile,
with `moqtap-codec` and MOQtail as dev-only test oracles; (b) depend on `moqtap-codec` (lighter
than MOQtail, and already has draft 23) and patch or upstream a parameter hook; (c) keep top-N
off the wire, implied by the profile and the plan.

**Decided: (a).** Our own codec for now; MOQtail or similar brought in for unit-test comparisons
and interop testing. Top-N is negotiated with a Palaver setup option (draft 22 section 6.3.2).

## Not covered

- Real audio (how a clipped or soft onset sounds; real level statistics; DTX behaviour of the
  encoder).
- Real transport timing and datagram loss patterns; FEC and DRED recovery.
- Overlay changes while people are speaking (a transit khef leaving).
- Untrusted levels and the conductor's priority overrides.
