# Spike 5 — media pipeline parts

Date: 2026-11-16. Throwaway code (`spikes/media`, `spikes/video`). Linux (WSL2) and Windows only.
Offline tests on Linux and Windows, plus one live microphone check through WSL.

## Questions and answers so far

| Question | Answer |
|---|---|
| Can `moq-audio` / `moq-video` be used without `moq-net` sessions? | **Yes, at the encoder/decoder level.** `moq_video::encode::Encoder` takes a raw frame and returns encoded payloads; `decode::Decoder` does the reverse. Driven directly in `spikes/video` with no session, 150 of 150 frames round-tripped. But `moq-net`, `hang` and `moq-mux` are compiled in regardless, and their types appear in the API (bitrate, timestamp, and the stream description the decoder needs, which comes from an async probe). 195 crates and a 222 MB debug binary for that one test |
| Does `moq-audio`'s Opus support DRED? | **No.** It uses `unsafe-libopus` 0.2, a machine translation of libopus 1.3 (2024). It sets DTX only: no FEC, no DRED |
| Is libwebrtc needed for audio processing? | **No.** `sonora` 0.3 (BSD-3) is a pure-Rust port of WebRTC's audio processing: echo canceller (AEC3), noise suppression, gain control. `moq-audio`'s `aec` feature is a wrapper around it. No C++ toolchain needed |
| Is there an Opus with DRED? | **Yes.** `opusic-sys` 0.7 bundles libopus **1.6.1** and builds it with cmake and a C compiler, with a `dred` feature |
| Capture and playback | `cpal` 0.18: WASAPI on Windows; ALSA and PulseAudio on Linux (see the live check below for what worked through WSLg) |
| Cross-compiling to Windows from Linux | **Works** with `cargo zigbuild` (zig as C compiler and linker), including libopus, `sonora`, `cpal` and `moq-video` with its Media Foundation backend. No Windows toolchain needed to build |

## Opus 1.6.1 measurements (Linux, 34 s of speech, 48 kHz mono, 20 ms, 32 kbit/s target)

| Mode | Mean packet | Max | Encode |
|---|---|---|---|
| Plain | 78 B | 105 B | 115 µs/frame |
| In-band FEC | 77 B | 127 B | 139 µs/frame |
| DRED, 500 ms of redundancy | 81 B | 125 B | 140 µs/frame |

FEC and DRED fit inside the bitrate target rather than adding to it, so packets stay near 80
bytes. DTX withheld only 1% of frames on this recording (it has no true silence); to be
re-measured with a live microphone.

Loss concealment, as a proxy: spectral distance (dB, lower is better) between lost frames as
reconstructed and the same frames with no loss.

| Loss pattern | Classic concealment | Neural concealment | FEC | DRED |
|---|---|---|---|---|
| 10% random single packets | 1.9 | 1.5 | **0.7** | 1.3 |
| 100 ms burst every second | 2.6 | 1.6 | 1.4 | **1.1** |
| 300 ms burst every 2 s | 3.7 | 3.1 | 3.0 | **2.3** |
| 600 ms burst every 3 s | 5.2 | 5.0 | 4.9 | 4.5 |

FEC is best for isolated losses, DRED for bursts up to its window. Decode cost with DRED or
neural concealment is about 30–50 µs per frame. This is an objective proxy, not a listening test,
and the source speech is narrowband (8 kHz recordings, upsampled).

## sonora measurements (Linux, simulated room)

Far-end speech played into a simulated room (40 ms delay, 120 ms of reflections, echo only
1.6 dB below the loudspeaker signal), background noise at −53 dBFS, near-end speech from 15 s.

| Configuration | Echo reduction (far end alone) | Residual | Cost |
|---|---|---|---|
| Echo canceller only | 28 dB | −57 dBFS | 74 µs per 10 ms |
| + noise suppression | 40 dB | −69 dBFS | 80 µs per 10 ms |

Noise suppression alone removed 15 dB of steady noise. During double talk the near speech
stayed within about 3 dB spectral distance of its echo-free version. A simulated room is
linear and has no clock drift between speaker and microphone; real devices are the real test.

## moq-video (Linux, software H.264 via openh264)

- 1280×720 at 30 fps, 1.5 Mbit/s target: 10 ms per frame to encode, 2 ms to decode (smooth
  synthetic content).
- Keyframe on demand (`Encoder::cut`) took effect on the same frame.
- With hard-to-compress content the software encoder skipped frames to hold the bitrate
  (63 of 150 delivered). Expected, but the pipeline must tolerate it.
- Compiled-in backends listed: VideoToolbox, Media Foundation, MediaCodec, NVENC, VAAPI, V4L2,
  openh264. Only openh264 could be exercised here (WSL has no GPU encoder or camera).

## Build environment notes

- No root on WSL, so cmake, ninja and zig came from Python packages in a local folder, and the
  ALSA development package was unpacked locally. For real development: `sudo apt install cmake
  clang libasound2-dev`.
- Debug build of the audio spike (Opus + sonora + cpal): 63 crates, 18 MB.

## Windows results (host, Smart App Control turned off by the user)

Cross-compiled from WSL and run on the Windows 11 host.

- **Opus and sonora**: same figures as Linux within noise (encode 115–140 µs per frame; echo
  reduction 28 dB / 40 dB; 74–80 µs per 10 ms).
- **H.264, 1280×720 at 30 fps** through `moq-video`:

| Encoder | Encode time | Decode time (Media Foundation) |
|---|---|---|
| Media Foundation (hardware; chosen automatically) | **0.7–0.8 ms/frame** | 0.4 ms/frame |
| openh264 (software) | 7.8 ms/frame | 0.4 ms/frame |

  Hardware encode takes about a tenth of the time of software. Keyframe on demand worked on
  both. Times are wall-clock for the call, not CPU or GPU load.
- **Smart App Control** blocked the unsigned test programs until it was turned off; it has no
  per-program allow list. **Unsigned Palaver builds will be blocked on machines that have it
  on, so code signing is a distribution requirement.**

## Live microphone check (WSL, USB headset, run by the user)

20 s captured through WSLg at 48 kHz: about 17 s of room silence, then a few seconds of speech (the user started speaking late).

| Observation | Detail |
|---|---|
| Capture works | 10.7 ms callbacks, first audio 27 ms after start |
| Levels | Silence about −91 dBFS; speech frames −60 to −37 dBFS (low gain on this path) |
| **Opus DTX did not engage on real silence** | All 505 frames of the quiet half were sent (mean 44 B), with or without DRED, in VOIP or AUDIO mode, at 24 or 32 kbit/s, and with the input amplified 30 dB. On digital silence DTX works: 33 of 505 frames sent (about 3 per second) |
| **A level gate ahead of the encoder fixes it** | Replacing frames below −70 dBFS (200 ms hangover) with digital silence: 40 of 505 frames sent in the quiet half |
| Noise suppression | Lowered the silent floor from −90 to −97 dBFS; speech level unchanged. Default gain control added no gain |
| Playback through ALSA routed to PulseAudio | Failed after the first buffer (underrun, then I/O errors). Capture on the same route was fine |
| `cpal`'s PulseAudio backend | Works from a normal shell, offering 44.1 kHz 16-bit; it had failed only inside the assistant's sandboxed shell. So the pipeline must resample and accept 16-bit devices |

Reading of this (agreed with the user): no design change. The microphone may simply be noisy
(breathing), Opus's own activity detection is likely sound, and top-N needs only that
something is sent with a level, not DTX specifically. We do not gate the signal to force DTX.
A selected but silent talker then costs up to 50 small packets a second on overlay links
instead of about 3; to watch, and to repeat natively on Windows in phase 1. The user heard the
tone, very short and late, so playback on this route partly worked.

Not tested: echo cancellation on real devices (headset only, no echo path); camera capture;
hardware decode into a renderer; the other platforms.

## Decisions (2026-11-16)

- **No libwebrtc.** Audio: `cpal` + `sonora` + libopus 1.6 with DTX, FEC and DRED.
- **Jitter buffer: our own**, with concealment handed to Opus. libwebrtc's NetEQ would mean
  building libwebrtc for one part; the `neteq` crate (0.9.1, MIT/Apache, about 8.4k lines; read,
  not run) has delay estimation and time-stretching to learn from but only a decode-only
  decoder hook, with no place for FEC or DRED.
- **Video: our own encoder/decoder interface**, openh264 first, **hardware backends early**
  (phase 2, Windows first) because the node needs its CPU for the khef. `moq-video`'s backends
  are the code to start from; we do not depend on the crate.
- To check before distribution: H.264 patent cover for a source-built openh264.

