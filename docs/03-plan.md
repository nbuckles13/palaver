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

- `moq-wire` (our own draft 22 codec for the profile, cross-checked against
  `moqtap-codec` in tests), `proto`, `khef-core`, `net` (QUIC + in-memory), `control` (reports and plan as
  tracks, flood rule), `sdk`, `bot`, `guards`.
- Ticket join (several entry points, multiple addresses per node); creator is conductor; real
  plan format; one audio overlay tree. Known limit until phase 3: if a node in the middle of
  the tree leaves, audio between the parts it joined stops until a new plan, and if it was the
  conductor there is none.
- `moq-wire` also needs subgroup streams in this phase: control tracks are too large for
  datagrams.
- Audio: capture → processing → Opus → datagram objects → khefs → jitter buffer → playout;
  top-N in its simplest form; synthetic sources.
- Simulator tests from day one, including a three-node line with no A–C link.
- Minimal egui app on the SDK. Playout in two channels, each voice heard where its picture
  is (design section 14).
  - Playout is driven by the speaker: the device asks for sound and the frames are made
    then. A timer beside the device is a second clock, and two clocks drift apart.
  - A check of the microphone (and later the camera) before joining, and again from
    inside a meeting: a meter, hearing yourself back, an input gain to set, and a hint
    when the microphone is very quiet.

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
- Hitless rebalancing of a working meeting, for latency and to move load off congested
  links: make-before-break for video routes and for the audio tree (design section 6).
- Graceful transit-khef leave/resignation, make-before-break; simulator invariant "zero lost
  objects" and a real-network audio-continuity test.
- Contingency next-hops and crash recovery with a measured bound; the second audio overlay tree
  (no node in the middle of both), so losing any one node leaves audio uninterrupted.
- Conductor hand-over within the 3 s cap; succession on crash.
- **Rate-control spike** (end of phase): throttle the middle link of a khef chain and the
  publisher uplink; compare receiver-side switching with khef-side track swap; decide the
  design for phase 4.

**Demo 3 (viability):** whoever carries the media — and the conductor — quits and nobody notices;
a machine is killed and the meeting recovers in under a second.

## Phase 4 — real discovery, rate control, scale to 25–50

- Open question for the password work: a PAKE stops offline guessing at admission, but
  anything else an outsider can see that is derived from the password (the meeting tag on
  the wire, announcements encrypted under the rendezvous key) can still be tested against
  guesses without dialling. Either keep those from outsiders or accept that Argon2id is
  their only protection.
- Meeting id + password: Argon2id/HKDF, PAKE admission; mDNS and DHT rendezvous;
  member-as-introducer punching; headless node as a reachable khef.
- Simulcast; priority shedding, congestion marking, rendition switching per the spike; uplink
  adaptation; caps and per-track routes; hardware H.264 on the remaining platforms.
- Audio adaptation to loss: turn Opus DRED on (and FEC off, or the bitrate up) on links that
  show bursts of loss; phase 1 sends FEC only.
- Audio bitrate and FEC budget: the phase 1 defaults (32 kbit/s, FEC planning for 15% loss)
  are placeholders nobody has listened to. FEC is paid for out of the bitrate, not added to
  it, and a higher loss setting moves more bits from the voice to its backup copy, so a
  constant 15% costs quality on a good link. To do: measure the copy's share of a packet at
  5%, 10% and 15%; listen to speech at 32 and 40 kbit/s with and without loss; pick fixed
  defaults from that; then set the loss figure from what each path measures, and raise the
  bitrate with protection where the link allows.
- Control hardening: plan deltas, convergence at 50 nodes.
- Emulated-network matrix; nightly soak; distributed bot runs (cloud VM added here).
- Frame encryption (prerequisite for any non-member relay), then optionally a public MoQ relay
  network for rendezvous and unreachable nodes.

**Demo 4:** 25–50 participants (humans plus bots) across home networks with only a meeting id and
password, surviving khef and conductor churn.

## Phase 5 — product

- Product UI shell (Flutter or Slint), installers, auto-update, accessibility.
- Moderation: remote mute enforced at the publisher's edge khef and upstream, kick, lock.
- Screen sharing, chat/data tracks, whiteboard. Shared computer sound and music as a track
  of two channels, without voice processing.
- SDK bindings (Python first); reference recorder and transcriber participants.
- Thin web client, if wanted.

## Phase 6 — mobile

- iOS/Android shells over the same SDK; platform voice processing; background rules.
- Phones run an edge khef only, or attach as thin clients; transit duty as a last resort.

## Backlog

Topics that need a design before they can be scheduled.

### Calling someone, and knocking

A ticket that is always on: its owner's node stays up, and whoever holds the ticket can
call. It is a meeting whose joiners knock and wait to be let in, which is also the host
approval the design defers, so the two should be designed together. What it needs beyond
a meeting: a way to find a node whose address has changed since the ticket was made
(rendezvous; the ticket coming back does not work when nobody is there to paste it), a
key that outlives one meeting, one ticket per contact so that one person can be cut off,
and a place in private mode, because being always on shows ticket holders when you are
online.

### Microphones the system has already processed

Windows can clean the microphone before an app gets it ("Voice Focus", "Voice Clarity").
Measured on a USB headset (2026-11-21), in the loudest quarter second of speech as
captured: -63 dB with both on, -44 dB with one off, -35 dB with both off. With either on,
the quiet between words was exact digital silence, and coughs and whistles never arrived.
Our own cleaning then works on top of someone else's: the level control lifted the
faintest of these by only 17 dB in thirty seconds, where it brings a voice at an ordinary
level to its target within seconds.

- Done: the microphone check notices it, before a meeting and during one, by the sound
  alone (dead silence between sounds, meaning under one step of 16 bits, which is how
  the recordings above were kept; nothing but dead silence; a voice that stays faint), which works the same on every system, and says where to look on the system it
  is on.
- To do, and particular to each system: ask the system itself instead of inferring it.
  Whether its voice effects are on for this device and whether the app is allowed the
  microphone at all (Windows and macOS each have their own way to ask); reading and
  setting the device's input level, so that a faint microphone can be raised from here;
  and choosing a microphone other than the default. To be checked on a Mac: what an app
  refused the microphone is given (silence, or an error on opening).
- Seen on a handheld's built-in microphone (2026-11-22; its choices are Off, Device
  Default Effects and Voice Clarity): with them off, the far end heard a pulsing hiss
  all the time, which a headset on another machine does not give. Not yet recorded or
  measured. The likely cause is that microphone's own noise, which the maker's effects
  were hiding, lifted by our level control because its voice is faint, with the pulse
  being the level control or the noise suppression hunting, or sending starting and
  stopping. A first recording, made with `loopback` through the handheld's own speaker,
  shows only that speaker feeding its microphone (a 9 kHz ring every 196 ms, the round
  trip of the loop) and settles nothing; between rings the microphone as it is sits at
  about -52 dB, and -60 dB or lower after our cleaning. To be recorded again with the
  speaker silent. So the advice cannot be "always turn the system's cleaning off": the check
  now says it often sounds better and to turn it back on if the microphone hisses. To
  do: record it (`loopback save` on that machine) and find which of ours is pulsing;
  stronger noise suppression when the level control is lifting a faint microphone.
- To find out: whether the level control can be made to climb faster for a faint voice
  (its settings, or a first guess at the gain from the first seconds of speech), without
  bringing up the noise of a quiet room when nobody is talking.

### The firewall on the machine that starts a meeting

Found on the first call between two machines (2026-11-21, two Windows machines on one
home network). The joiner needed nothing: a firewall lets in the answers to what its own
machine sent. The creator was unreachable, silently, for two reasons that each sufficed:
Windows had no rule for the program (and showed no prompt asking for one), and the home
network was marked Public. With a rule for the program and the network marked Private,
the call was good at once.

- **The installer adds the rule** (decided 2026-11-21): one rule for our program,
  incoming UDP, on every kind of network; named, said in the installer, and taken away
  on uninstall; through the firewall's own interface. Every kind of network because home
  networks are often marked Public and a meeting that fails there fails without a word;
  what it exposes is a port that refuses anyone who does not know the meeting's secret.
- **The app notices** when it has been in a meeting for a while and nobody has ever
  reached it, says what that usually means, and offers to add the rule (one prompt for
  permission). This covers installs without administrator rights and the bare program.
- It cannot cover other firewalls or a company's policy. The way not to depend on a rule
  is for both ends to send first, which is what rendezvous (phase 4) brings to the one
  case that needs it: reaching the creator, who does not yet know the joiner exists.
- Heard on that call: the delay is low enough that, with both machines in one room, the
  talker's own voice and its copy from the other machine are hard to tell apart.

### Noise suppression: how much, and who decides

What there is (`media::process`): WebRTC's suppressor as ported in `sonora`, at its
default strength. It turns down steady noise (a fan, a hum, hiss: more than 10 dB in the
tests) and does little for sudden sounds: a keyboard, a dog, someone else talking in the
room. Tested on made-up hiss only, not on a real fan or a street.

- **Settings for the user.** At the least: noise suppression on or off, and how strong;
  echo cancelling and level control on or off. They belong with the microphone check,
  where the effect can be heard at once. A musician, or someone whose system already
  cleans the microphone, wants them off.
- **Stronger suppression**, if the default proves too little once it can be heard in the
  app: `sonora`'s higher levels (more noise gone, at some cost to the voice), or a neural
  suppressor (RNNoise, DeepFilterNet) as a stage of its own, which is what goes after
  the sudden sounds.
- To measure first: real noise, and what each level costs the voice.

### Echo cancelling on real devices

With headphones there is no echo, and the canceller should do nothing. Measured on a real
headset with the system's own voice effects off (2026-11-21, twenty seconds against a
made-up other voice): nothing of the other voice came through, and the talker came out
as loud while the other voice played as alone, but for one dip of 8 dB three seconds
in. An earlier run that showed the talker 3 to 20 dB down was through the system's
effects and a clipping gain, and does not count.

One call with speakers so far (2026-11-21, the handheld's own, against a headset at the
other end): no echo was heard, but its microphone is faint enough to need a voice close
to it, so little of the speaker's sound can have reached it. Not a hard test.

Not yet measured: speakers at an ordinary level, where there is an echo to remove. Until it is, keep in mind
the ways out if it disappoints: tuning the canceller, turning it off when the output is
headphones (where the system can say), and a switch for the user.

### How little delay

Read on a call between two Windows machines on one home network (2026-11-21): each
voice's jitter buffer at 40 to 60 ms, nothing late, under 1% lost and nearly all of that
rebuilt. The delay we add is then mostly two waits that do overlapping work: the jitter
buffer's floor (two frames, 40 ms) and the sound kept waiting for the speaker (40 ms). A
frame decoded and waiting for the speaker is already safe from a late packet behind it.

- Tried with no floor, no margin and a lead of 20 ms on the same two machines: heard
  60 ms after arriving, nothing lost or late. Those are now what `palaver-harness` starts
  with (`--floor`, `--margin` in frames and `--lead` in ms still set them).
- The buffer starts at two frames whatever its floor, so the first seconds are covered
  while it learns the network, and comes down from there. It comes down without
  dropping speech where it can: in a pause it moves straight to its target, as far as
  the silence reaches; between pauses it skips a quiet frame, and a voiced one only
  after waiting two seconds for a quiet one.
- To do: playing a little faster or slower without changing the pitch (taking out or
  repeating whole periods of the voice, as WebRTC's buffer does). It catches up without
  waiting for a pause, grows the delay mid-sentence without a gap, and lets the delay be
  set finer than one 20 ms frame, which is what stands between a good network and a
  buffer of nearly nothing: a few milliseconds of spread now round up to a whole frame.
- To decide: whether a voiced frame should ever be skipped to come down, or the buffer
  should always wait for the pause.
- Further off: frames of 10 ms, which halve the wait to fill a frame and let the buffer
  move in smaller steps, for more packets a second.

Not pursued: Windows runs timers about 15 ms late unless asked for finer ones (seen in
the details on both machines). Nothing on the sound's way is driven by a timer, and the
jitter buffer takes up what it adds elsewhere. To revisit only if a gauge points at it.

### Remembering each participant's delay

There is one jitter buffer and one decoder per voice received, kept by the track's alias,
which is the same for a track across the whole meeting and which every packet carries;
the plan says whose track it is. A voice silent for five seconds is forgotten, buffer and
all, and one that speaks again starts from the starting delay.

Because each voice has its own buffer and decoder and the voices are mixed, a change of
talker needs no swap: what is left of A in its buffer plays out while B's buffer fills to
its starting delay, and B joins the mix when it is ready. Nothing of A's tail is dropped,
and no decoder is handed a different voice. This is to be kept (and tested) however the
buffers are managed later.

To do: keep the delay each voice had reached after the voice is dropped, whether for
silence or because it left the loudest few, and when it comes back wait for that much
before playing, adjusting from there. A voice never heard before starts from the
defaults, never from what another voice's buffer has reached.

- Kept by track alias: one per participant and kind of sound (microphone, shared
  audio), which is the right grain, since each has its own source and may take its own
  path. Forgotten when the plan drops the track, so that an alias given out again
  brings nothing with it.
- Trusted for 30 to 60 seconds, then back to the defaults: the path may have changed.
  The case to serve is voices taking turns, where each would otherwise be learnt
  again every time round.

### A sender whose clock runs fast

The jitter buffer takes a steady surplus of frames for network jitter: its target delay
climbs (to its 500 ms ceiling, seen with a microphone delivering 8% too much through
WSLg) and it sheds the surplus only once the delay is 200 ms over the target. Real
devices differ by hundredths of a percent, where none of this shows. To do: keep the
target where it was when the lateness is a trend and not a scatter, and shed sooner.

### The statistics panel

A first pass is in: the details slide out beside the meeting (the window grows by their
width), in three parts: this machine, each direct link, each voice. Each has its numbers
as they stand and, for the ones worth watching, a line over the last two minutes, one
value a second: the sound thread's longest wait; a link's round trip and packets lost a
second; a voice's delay from arriving to being heard and its frames lost or late a
second. The history is kept by the session and only read by the window. A voice that
stops stays listed, with a gap in its line, until it has been silent two minutes.

Each plot has a scale down its side and says the value under the pointer. In a window
that fills its screen the details take their width from the meeting; otherwise the
window grows for them.

To decide: a voice's plots stop when its sender is muted or silent, because nothing is
sent then and there is nothing to measure. A small packet now and then through silence
(Opus makes one every 400 ms; we drop it today) would keep the measurements going and
the far end's jitter buffer warm, for a little traffic from every silent participant.

To do:

- Gauges for the numbers as they stand, in place of sentences; the sentences are a
  developer's.
- A plot's line only says its scale in words. Marks along its edge, and the value under
  the pointer.
- More to draw as it comes: the speaker's lead, what the microphone sends, the plan's
  changes, each hop of a voice that came by more than one.
- Names: a link line is for a direct connection, and a voice line for a voice however
  many hops it came. To be made plain on the page.

### What stands for someone whose camera is off

To settle when video comes: what a tile shows for a participant with no picture (camera
off, or none sent to this node). Candidates: the name, initials on a disc as in the
circle today, an icon, a picture the participant chose. A chosen picture has to be
carried to everyone, which the others do not.

### Bots that say words

For manual testing by ear, a bot that says something recognisable ("alpha, alpha, alpha")
rather than a tone with a rhythm, so a person listening can tell bots apart and hear
clipping. Needs recorded or synthesised speech as a source.

### Post-mortem debugging of packet flows

Someone says "I couldn't hear Carol for ten seconds around 10:03". With no server, the only
evidence is what each node kept. We need to be able to work out, afterwards and from the
nodes' own records, where the packets went and where they stopped. Much of what decides a
packet's fate is dynamic, so the records must let it be reconstructed:

- **Alias to track, over time.** Aliases are assigned by the conductor and change with the
  plan. A packet-level record is only readable alongside the plan that was in force.
- **The path, over time.** Which tree or route a track took under each plan generation, and
  when each node applied each plan (nodes apply a plan at different moments).
- **Per hop, per track**: received, forwarded, and dropped with the reason (not selected by
  top-N, unknown alias, duplicate, late, queue full, stream reset, link down).
- **Top-N decisions**: who was selected on each link and when it changed.
- **Link history**: up, down, silent, fillers; and the roster's view of it, with holds.
- **At the receiver**: what the jitter buffer did (rebuilt, concealed, skipped, held) and
  what the decoder was given.
- **The control plane's own traffic**: which reports and plans arrived where and when.

Questions to settle:

- **Clocks.** Nodes share no clock. How are records from several nodes lined up: offsets
  measured between neighbours, or a common reference carried in reports?
- **Joining several nodes' records** into one account of the meeting, with redaction
  intact: the same participant must be the same token in every node's bundle, without the
  token revealing who it is.
- **Cost.** What can be kept always (counters, changes of state) and what only on demand
  or for the last few minutes (per-packet records); no media payload, ever.
- **One format** for the simulator's trace and a real node's, so the same tool reads both
  and a real failure can be replayed in the simulator.
- **Who can collect it.** A participant can export their own node's bundle; whether a host
  can ask others for theirs, and what consent that needs.

## Sequencing notes

- Headless before GUI in every phase.
- Phase 3 holds the novel risk, so it precedes real discovery; if the transport spike shows
  first-contact NAT traversal is bad, pull discovery forward.
- Windows and macOS build in CI from phase 0; real-device media is validated on the Windows host
  and the MacBook.
