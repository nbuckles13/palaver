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
  requests, tiles. In this order (agreed 2026-11-22):
  1. Camera to window on one machine. Begun: the picture type, a moving pattern, and a
     tile in the window that shows this machine's own picture. Capture is next, behind
     a crate of our own.
  2. Encode and decode in a loop on one machine, behind our own interface, with the
     pattern as a source for tests. Done in software (openh264); to be tried on the
     Windows machines, and on a camera's pictures, which are harder to code than the
     pattern's.
  3. One video track across a link, at one size: streams in the wire code, subscribe
     by name, keyframe requests. In four pieces:
     a. The session: a single track asked for by name, its groups passed on as they
        arrive, a new group asked for. Done.
     b. The khef: who is asked for a track (to begin with, the neighbour on the audio
        tree's path to its publisher), a subscription carried hop by hop, groups
        forwarded, requests for a new group passed up once for many askers, and a track
        produced only while someone wants it. Done.
     c. The node and the SDK: video tracks in the plan, a sender that makes groups from
        coded frames and a receiver that turns them back into pictures. Done.
     d. The app and the bot: the pattern sent, and a tile for someone else's picture.
        Done; to be tried between machines.
  4. Tiles for everyone (done: everyone's one size, each decoded), then simulcast and
     choosing a size for each tile, in three pieces:
     a. What a camera offers said in full in the report and the plan, and changed
        while the meeting lasts. Done.
     b. Pictures made smaller, and a sender for each size on offer (180p at 150 kbit/s,
        360p at 500, 720p at 1500, each made only while watched). Done.
     c. Each tile watches the smallest size that covers it, and changes over without
        a gap; the bot offers all three. Done: the size follows how large the
        picture is drawn and nothing else; following what the network can carry
        comes with the work on bad networks.
  5. Hardware encoding on Windows.
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
  all the time, which a headset on another machine does not give. Recorded with its
  speaker silent: the microphone's own noise is at -40 dB, mostly below 500 Hz, and its
  voice only some 10 dB above that. Our cleaning brings the noise to -47 dB and holds
  it steady, so the pulse is not made there. Run through the encoder, that noise is
  sent in stretches of a few hundred milliseconds with nothing between: the encoder
  cannot settle whether it is worth sending, and the far end, which plays silence when
  nothing comes, hears it come and go. With noise suppression at its "high" setting
  (18 dB, against 12) the same recording is withheld cleanly between words, but for
  the small packet Opus sends every 400 ms, which may itself be heard as a tick over
  silence. (An earlier recording through that machine's own speaker showed only the
  speaker feeding the microphone, and does not count.)
- So the advice cannot be "always turn the system's cleaning off": the check says it
  often sounds better, and to turn it back on if the microphone hisses.
- To do, in this order: stronger noise suppression, at least for a microphone this
  noisy, judged by ear for what it does to a voice; and at the far end a quiet noise
  kept up through silence in place of nothing, so that what is sent and what is
  withheld do not sound so different.
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
default strength ("moderate", about 12 dB) unless told otherwise: `Settings` carries
the strength, and `palaver-harness --noise low|moderate|high|veryhigh` sets it, to try
by ear. It turns down steady noise (a fan, a hum, hiss: more than 10 dB in the
tests) and does little for sudden sounds: a keyboard, a dog, someone else talking in the
room. Tested on made-up hiss only, not on a real fan or a street.

- **Settings for the user** (wanted, 2026-11-22). At the least: noise suppression on or
  off, and how strong; echo cancelling and level control on or off. They belong with the microphone check,
  where the effect can be heard at once. A musician, or someone whose system already
  cleans the microphone, wants them off.
- **Chosen for the user where it can be** (done): tried by hand first, `--noise high` on
  the handheld with its system's cleaning off took the pulsing hiss away (2026-11-22,
  by ear). Now the check's ear says when a microphone is noisy (the quietest tenth of
  five seconds at -50 dB or above), and the sound thread then turns suppression up to
  "high" for the rest of the call, without starting the processor again. `--noise`
  with a strength fixes it; `--noise auto` is the default. The details say when it is
  on. To find out: what "high" costs a voice, where the line between a noisy
  microphone and an ordinary one should be (one noisy and no quiet one measured so
  far), and whether it should come down again.
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

### Routing video apart from audio

To begin with a video track is asked for along the audio tree: the neighbour on the
tree's path to its publisher. That needs nothing new in the plan, and it is wrong for
anything but small meetings: it puts every picture on the links that carry every
voice.

Wanted (2026-11-23): video routed in step with audio and apart from it. Audio gets the
best path, for delay and for room; video is fitted in around it, to spread the load
between machines and to use each path for what it can carry. The conductor already
gathers what this needs (each node's links and their measurements) and the design
already has the place for the answer (the plan's routing table, section 7: a default
route for each participant, finer entries where wanted, more than one next hop).

To work out:

- What a route is chosen by: what a link can carry beyond its audio, how many pictures
  already cross a machine, how many hops.
- Congestion: what a khef does when a link cannot take what is routed over it (a
  smaller size, fewer pictures, another path), who decides, and how fast; with rate
  control in phase 4.
- Changing a route under a picture being watched, with nothing lost: the new path up
  before the old is dropped, as is designed for audio.

### Video waiting for a slow link

A group of pictures goes to each subscriber on a stream. Between the khef and each
stream is a queue with no end to it (`sdk::engine`: one per stream, filled as fast as
the pictures arrive and emptied as fast as the link takes them). A neighbour that
takes less than is sent sees its pictures fall further and further behind, and the
sender's memory grows. Sound is not at risk in the same way: it goes in datagrams,
which are dropped when they cannot go.

What is to be done about it is in the design (section 7, "When a link cannot carry its
video"): the khef at the link sheds at once, the subscriber asks for a smaller size, the
conductor routes or limits. It comes after video runs well on a good network (decided
2026-11-25), and with test suites for each rule. First of it: a limit on how long
anything may wait for one link, the group abandoned on that link when it is passed,
nothing more of the track sent there until the link has drained, and a count of each
time. Not to be mended by asking for a key frame, which is the largest thing to send.
Also to be designed then: going back up when a link recovers or a new way is given,
which needs probing and an estimate of what a link can carry above what it carries.

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

A voice's plots stop when its sender is muted: nothing at all is sent then, and there
is nothing to measure. A sender who is only silent still sends the small packet Opus
makes every 400 ms, which keeps the voice listed and its buffer current. To decide:
whether a muted sender should send the same, for the measurements' sake.

To do:

- Gauges for the numbers as they stand, in place of sentences; the sentences are a
  developer's.
- A plot's line only says its scale in words. Marks along its edge, and the value under
  the pointer.
- More to draw as it comes: the speaker's lead, what the microphone sends, the plan's
  changes, each hop of a voice that came by more than one.
- Video, once it crosses a network, in place of today's "your picture" and "as sent"
  lines (which are of a loop on one machine): for what this machine sends, the time to
  encode and the rate sent, for each size; for each picture received, the rate
  received, the time to decode, and how long from a frame's arrival to its being on
  the screen (the like of a voice's "heard after arriving"), with frames lost, late or
  let go and key frames asked for. The same two kinds of line as sound has: one for
  this machine, one for each track received.
- Further off (wanted, 2026-11-23): end-to-end numbers, from a camera or a microphone
  on one machine to the screen or the speaker on another. What is shown today starts
  at a frame's arrival, because two machines' clocks are not compared. Ways to get
  there: compare the clocks over the link (as the round trip is already measured), or
  measure from outside with a pattern that carries the time and a sound made at a
  known moment.
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
