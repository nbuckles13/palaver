//! A safe wrapper over libopus, for one use: mono speech at 48 kHz in 20 ms frames.
//!
//! This is the one crate that talks to the Opus C library, and so the one crate that may use
//! `unsafe` for it. Nothing unsafe leaves it: every pointer is owned by a type here and freed
//! on drop, every length is checked before C sees it, and every C return code becomes a
//! `Result`. libopus takes no callbacks, so nothing of ours runs on its stack.
//!
//! The decoder offers the frame itself and the three ways of filling a frame that did not
//! arrive, matching what the jitter buffer can ask for:
//!
//! - [`Decoder::decode`]: the packet is here;
//! - [`Decoder::decode_fec`]: the packet is lost but the next one is here, and carries a
//!   coarse copy of it (in-band FEC);
//! - [`Decoder::decode_dred`]: a later packet is here, and carries up to a second of what
//!   came before it (DRED);
//! - [`Decoder::conceal`]: nothing is here, guess.
#![allow(unsafe_code)]

use core::ffi::c_int;
use core::ptr::NonNull;
use opusic_sys as sys;

/// The only sample rate used.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples in one 20 ms frame.
pub const FRAME_SAMPLES: usize = 960;
/// The largest packet the encoder will produce.
pub const MAX_PACKET_BYTES: usize = 1275;
/// A packet this short or shorter means "nothing worth sending" (discontinuous
/// transmission): the encoder found silence.
pub const DTX_PACKET_BYTES: usize = 2;
/// The most frames back that DRED can reach.
pub const MAX_DRED_FRAMES: u32 = 50;

/// Why a call into libopus failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A frame of audio must be exactly [`FRAME_SAMPLES`] long.
    FrameLength,
    /// The output buffer or the packet is too long for libopus to be told its length.
    TooLong,
    /// libopus refused, with this code: the packet is corrupt, a setting is not supported
    /// by this build, or memory ran out.
    Opus(i32),
}

fn check(code: c_int) -> Result<c_int, Error> {
    if code < 0 {
        Err(Error::Opus(code))
    } else {
        Ok(code)
    }
}

fn len(value: usize) -> Result<c_int, Error> {
    c_int::try_from(value).map_err(|_| Error::TooLong)
}

/// The version of libopus this was built with.
pub fn version() -> String {
    // SAFETY: opus_get_version_string returns a pointer to a static NUL-terminated string.
    let text = unsafe { core::ffi::CStr::from_ptr(sys::opus_get_version_string()) };
    text.to_string_lossy().into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Target bitrate, in bits per second.
    pub bitrate: u32,
    /// Send nothing during silence.
    pub dtx: bool,
    /// Carry a coarse copy of each frame in the packet after it.
    pub fec: bool,
    /// How many frames of history each packet carries for DRED; 0 turns it off, at most
    /// [`MAX_DRED_FRAMES`].
    pub dred_frames: u32,
    /// The loss the encoder should plan for, in percent. FEC and DRED are only spent on
    /// when this is above zero.
    pub expected_loss_percent: u8,
}

impl Default for EncoderConfig {
    /// 32 kbit/s, DTX and FEC on, up to half a second of DRED, planning for 15% loss.
    ///
    /// FEC and DRED are paid for out of the bitrate, and the encoder decides frame by frame
    /// what it can afford. Measured with libopus 1.6.1 on a voice-like test signal, how far
    /// back a packet's DRED history reached:
    ///
    /// | Bitrate | Loss planned for | FEC and DRED | DRED only |
    /// |---|---|---|---|
    /// | 24 kbit/s | 15% | none | 95 ms |
    /// | 32 kbit/s | 15% | none | 135 ms |
    /// | 32 kbit/s | 30% | 95 ms | 215 ms |
    /// | 48 kbit/s | 15% | 95 ms | 375 ms |
    /// | 48 kbit/s | 30% | 415 ms | 495 ms |
    /// | 64 kbit/s | 15% | 175 ms | 495 ms |
    ///
    /// So at this default the packets carry FEC and no DRED. Which of FEC, DRED or a higher
    /// bitrate serves a meeting best is to be settled by listening, not here.
    fn default() -> Self {
        Self {
            bitrate: 32_000,
            dtx: true,
            fec: true,
            dred_frames: 25,
            expected_loss_percent: 15,
        }
    }
}

/// An Opus encoder for mono speech.
#[derive(Debug)]
pub struct Encoder {
    raw: NonNull<sys::OpusEncoder>,
}

// SAFETY: the encoder state is owned by this value alone and libopus keeps no thread-local
// or global state for it, so it may move to another thread. It is not `Sync`: every method
// that touches the state takes `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, Error> {
        let mut error: c_int = 0;
        // SAFETY: valid arguments (48 kHz, one channel, a defined application) and a valid
        // pointer for the error code. The result is checked for null below.
        let raw = unsafe {
            sys::opus_encoder_create(
                SAMPLE_RATE as i32,
                1,
                sys::OPUS_APPLICATION_VOIP as c_int,
                &mut error,
            )
        };
        check(error)?;
        let encoder = Self {
            raw: NonNull::new(raw).ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))?,
        };
        let dred_frames = config.dred_frames.min(MAX_DRED_FRAMES);
        // DRED's duration is given in 10 ms units.
        let settings = [
            (
                sys::OPUS_SET_BITRATE_REQUEST,
                config.bitrate.min(510_000) as c_int,
            ),
            (sys::OPUS_SET_DTX_REQUEST, c_int::from(config.dtx)),
            (sys::OPUS_SET_INBAND_FEC_REQUEST, c_int::from(config.fec)),
            (
                sys::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
                c_int::from(config.expected_loss_percent.min(100)),
            ),
            (
                sys::OPUS_SET_DRED_DURATION_REQUEST,
                (dred_frames.saturating_mul(2)) as c_int,
            ),
        ];
        for (request, value) in settings {
            // SAFETY: `raw` is a live encoder, and each of these requests takes one
            // `opus_int32` argument, which is what is passed.
            check(unsafe { sys::opus_encoder_ctl(encoder.raw.as_ptr(), request, value) })?;
        }
        Ok(encoder)
    }

    /// Encode one frame. Returns how many bytes of `packet` were written. A result of
    /// [`DTX_PACKET_BYTES`] or fewer means the frame was silence and nothing need be sent.
    pub fn encode(&mut self, pcm: &[f32], packet: &mut [u8]) -> Result<usize, Error> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(Error::FrameLength);
        }
        let capacity = len(packet.len().min(MAX_PACKET_BYTES))?;
        // SAFETY: `raw` is a live encoder; `pcm` holds exactly the `FRAME_SAMPLES` samples
        // libopus is told to read; `packet` has at least `capacity` bytes to write into.
        let written = unsafe {
            sys::opus_encode_float(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                FRAME_SAMPLES as c_int,
                packet.as_mut_ptr(),
                capacity,
            )
        };
        Ok(check(written)? as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from opus_encoder_create and is destroyed exactly once, here.
        unsafe { sys::opus_encoder_destroy(self.raw.as_ptr()) }
    }
}

/// An Opus decoder for mono speech, with its DRED state.
#[derive(Debug)]
pub struct Decoder {
    raw: NonNull<sys::OpusDecoder>,
    dred_decoder: NonNull<sys::OpusDREDDecoder>,
    dred: NonNull<sys::OpusDRED>,
}

// SAFETY: as for `Encoder`: the three states are owned by this value alone, libopus keeps
// nothing thread-local for them, and every method that touches them takes `&mut self`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, Error> {
        let mut error: c_int = 0;
        // SAFETY: valid arguments and a valid pointer for the error code; checked for null.
        let raw = unsafe { sys::opus_decoder_create(SAMPLE_RATE as i32, 1, &mut error) };
        check(error)?;
        let raw = NonNull::new(raw).ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))?;
        // From here on, a failure must free what was made: `partial` does that on drop.
        let mut partial = Partial {
            raw: Some(raw),
            dred_decoder: None,
            dred: None,
        };
        // SAFETY: a valid pointer for the error code; checked for null.
        let dred_decoder = unsafe { sys::opus_dred_decoder_create(&mut error) };
        check(error)?;
        partial.dred_decoder =
            Some(NonNull::new(dred_decoder).ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))?);
        // SAFETY: a valid pointer for the error code; checked for null.
        let dred = unsafe { sys::opus_dred_alloc(&mut error) };
        check(error)?;
        partial.dred = Some(NonNull::new(dred).ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))?);
        partial.finish().ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))
    }

    fn check_output(pcm: &[f32]) -> Result<(), Error> {
        if pcm.len() == FRAME_SAMPLES {
            Ok(())
        } else {
            Err(Error::FrameLength)
        }
    }

    fn decode_raw(
        &mut self,
        packet: Option<&[u8]>,
        fec: bool,
        pcm: &mut [f32],
    ) -> Result<(), Error> {
        Self::check_output(pcm)?;
        let (data, length) = match packet {
            Some(packet) => (packet.as_ptr(), len(packet.len())?),
            None => (core::ptr::null(), 0),
        };
        // SAFETY: `raw` is a live decoder; `data` is either null with length 0 (which asks
        // for concealment) or points to `length` readable bytes; `pcm` has room for the
        // `FRAME_SAMPLES` samples libopus is told it may write.
        let samples = unsafe {
            sys::opus_decode_float(
                self.raw.as_ptr(),
                data,
                length,
                pcm.as_mut_ptr(),
                FRAME_SAMPLES as c_int,
                c_int::from(fec),
            )
        };
        if check(samples)? as usize == FRAME_SAMPLES {
            Ok(())
        } else {
            // A packet that does not hold exactly one 20 ms frame is not one of ours.
            Err(Error::Opus(sys::OPUS_INVALID_PACKET))
        }
    }

    /// Decode the frame in `packet` into `pcm`.
    pub fn decode(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<(), Error> {
        self.decode_raw(Some(packet), false, pcm)
    }

    /// The frame before `next_packet` was lost: rebuild it from the copy `next_packet`
    /// carries. If it carries none, libopus conceals instead. Decode `next_packet` itself
    /// afterwards with [`Decoder::decode`].
    pub fn decode_fec(&mut self, next_packet: &[u8], pcm: &mut [f32]) -> Result<(), Error> {
        self.decode_raw(Some(next_packet), true, pcm)
    }

    /// How many whole frames back the DRED history in `packet` reaches; 0 if it carries
    /// none.
    pub fn dred_reach(&mut self, packet: &[u8]) -> Result<u32, Error> {
        let mut dred_end: c_int = 0;
        // SAFETY: both DRED states are live; `packet` is readable for the length given;
        // `dred_end` is a valid place for the result.
        let available = unsafe {
            sys::opus_dred_parse(
                self.dred_decoder.as_ptr(),
                self.dred.as_ptr(),
                packet.as_ptr(),
                len(packet.len())?,
                SAMPLE_RATE as i32,
                SAMPLE_RATE as i32,
                &mut dred_end,
                0,
            )
        };
        Ok((check(available)? as usize / FRAME_SAMPLES) as u32)
    }

    /// Nothing to go on: produce a frame that continues the sound so far, fading out if it
    /// goes on.
    pub fn conceal(&mut self, pcm: &mut [f32]) -> Result<(), Error> {
        self.decode_raw(None, false, pcm)
    }

    /// A frame was lost, and `later_packet`, `frames_back` frames after it, is here: rebuild
    /// the lost frame from the history that packet carries. Returns `false`, writing
    /// nothing, if the packet's history does not reach that far back; conceal instead.
    ///
    /// How far a packet's history reaches is the encoder's choice, frame by frame, and is
    /// often much less than [`EncoderConfig::dred_frames`] allows: see [`Decoder::dred_reach`].
    pub fn decode_dred(
        &mut self,
        later_packet: &[u8],
        frames_back: u32,
        pcm: &mut [f32],
    ) -> Result<bool, Error> {
        Self::check_output(pcm)?;
        if frames_back == 0 || frames_back > MAX_DRED_FRAMES {
            return Ok(false);
        }
        let offset = (frames_back as usize).saturating_mul(FRAME_SAMPLES);
        let offset = len(offset)?;
        let mut dred_end: c_int = 0;
        // SAFETY: both DRED states are live; `later_packet` is readable for the length
        // given; `dred_end` is a valid place for the result.
        let available = unsafe {
            sys::opus_dred_parse(
                self.dred_decoder.as_ptr(),
                self.dred.as_ptr(),
                later_packet.as_ptr(),
                len(later_packet.len())?,
                // Ask for all the history the packet has (up to a second); asking for only
                // as much as is needed can come back a little short of it.
                SAMPLE_RATE as i32,
                SAMPLE_RATE as i32,
                &mut dred_end,
                0,
            )
        };
        if check(available)? < offset {
            return Ok(false);
        }
        // SAFETY: the decoder and the DRED state just filled are live; `pcm` has room for
        // the `FRAME_SAMPLES` samples libopus is told it may write.
        let samples = unsafe {
            sys::opus_decoder_dred_decode_float(
                self.raw.as_ptr(),
                self.dred.as_ptr(),
                offset,
                pcm.as_mut_ptr(),
                FRAME_SAMPLES as c_int,
            )
        };
        Ok(check(samples)? as usize == FRAME_SAMPLES)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: each pointer came from its matching create call and is freed exactly
        // once, here.
        unsafe {
            sys::opus_dred_free(self.dred.as_ptr());
            sys::opus_dred_decoder_destroy(self.dred_decoder.as_ptr());
            sys::opus_decoder_destroy(self.raw.as_ptr());
        }
    }
}

/// The parts of a decoder made so far, freed on drop unless it was finished.
struct Partial {
    raw: Option<NonNull<sys::OpusDecoder>>,
    dred_decoder: Option<NonNull<sys::OpusDREDDecoder>>,
    dred: Option<NonNull<sys::OpusDRED>>,
}

impl Partial {
    fn finish(mut self) -> Option<Decoder> {
        Some(Decoder {
            raw: self.raw.take()?,
            dred_decoder: self.dred_decoder.take()?,
            dred: self.dred.take()?,
        })
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        // SAFETY: each pointer still held came from its matching create call and has not
        // been handed on (a finished decoder takes them all), so it is freed exactly once.
        unsafe {
            if let Some(dred) = self.dred {
                sys::opus_dred_free(dred.as_ptr());
            }
            if let Some(dred_decoder) = self.dred_decoder {
                sys::opus_dred_decoder_destroy(dred_decoder.as_ptr());
            }
            if let Some(raw) = self.raw {
                sys::opus_decoder_destroy(raw.as_ptr());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice-like signal: a pitch with harmonics, its loudness rising and falling a few
    /// times a second.
    fn voice(frames: usize) -> Vec<f32> {
        (0..frames * FRAME_SAMPLES)
            .map(|n| {
                let t = n as f32 / SAMPLE_RATE as f32;
                let envelope = 0.5 + 0.5 * (2.0 * core::f32::consts::PI * 3.0 * t).sin();
                let tone: f32 = (1..=8)
                    .map(|h| (2.0 * core::f32::consts::PI * 140.0 * h as f32 * t).sin() / h as f32)
                    .sum();
                0.25 * envelope * tone
            })
            .collect()
    }

    fn energy(pcm: &[f32]) -> f32 {
        pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32
    }

    fn encode_all(config: EncoderConfig, pcm: &[f32]) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new(config).unwrap();
        let mut packet = [0u8; MAX_PACKET_BYTES];
        pcm.chunks_exact(FRAME_SAMPLES)
            .map(|frame| {
                let n = encoder.encode(frame, &mut packet).unwrap();
                packet[..n].to_vec()
            })
            .collect()
    }

    #[test]
    fn it_is_libopus_one_point_five_or_later() {
        let version = version();
        assert!(version.contains("libopus"), "{version}");
        // DRED needs 1.5; the encoder accepts the setting only if it is built in.
        assert!(Encoder::new(EncoderConfig::default()).is_ok(), "{version}");
    }

    #[test]
    fn speech_survives_a_round_trip_at_about_the_target_bitrate() {
        let pcm = voice(100);
        let packets = encode_all(EncoderConfig::default(), &pcm);
        assert_eq!(packets.len(), 100);
        let bytes: usize = packets.iter().map(Vec::len).sum();
        // 32 kbit/s for two seconds is 8000 bytes; FEC and DRED are inside that.
        assert!((5000..=9000).contains(&bytes), "{bytes} bytes");

        let mut decoder = Decoder::new().unwrap();
        let mut out = vec![0.0f32; pcm.len()];
        for (packet, frame) in packets.iter().zip(out.chunks_exact_mut(FRAME_SAMPLES)) {
            decoder.decode(packet, frame).unwrap();
        }
        // Skip the first frames, while the codec settles.
        let (sent, got) = (energy(&pcm[9600..]), energy(&out[9600..]));
        assert!(
            (0.5..=1.5).contains(&(got / sent)),
            "energy in {sent}, out {got}"
        );
    }

    #[test]
    fn silence_is_not_worth_sending() {
        let silence = vec![0.0f32; 100 * FRAME_SAMPLES];
        let packets = encode_all(EncoderConfig::default(), &silence);
        let withheld = packets
            .iter()
            .filter(|p| p.len() <= DTX_PACKET_BYTES)
            .count();
        assert!(withheld >= 80, "{withheld} of 100 frames withheld");
        // With DTX off every frame is sent.
        let config = EncoderConfig {
            dtx: false,
            ..EncoderConfig::default()
        };
        let packets = encode_all(config, &silence);
        assert!(packets.iter().all(|p| p.len() > DTX_PACKET_BYTES));
    }

    /// Decode `packets`, with frame `lost` missing and filled in by `fill`. Returns that
    /// frame as decoded.
    fn with_one_lost(
        packets: &[Vec<u8>],
        lost: usize,
        fill: impl Fn(&mut Decoder, &mut [f32]),
    ) -> Vec<f32> {
        let mut decoder = Decoder::new().unwrap();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        let mut filled = vec![];
        for (i, packet) in packets.iter().enumerate() {
            if i == lost {
                fill(&mut decoder, &mut frame);
                filled = frame.clone();
            } else {
                decoder.decode(packet, &mut frame).unwrap();
            }
        }
        filled
    }

    /// FEC off, so that at 32 kbit/s the packets carry DRED.
    fn dred_only() -> EncoderConfig {
        EncoderConfig {
            fec: false,
            ..EncoderConfig::default()
        }
    }

    #[test]
    fn a_lost_frame_is_filled_by_fec_by_dred_and_by_concealment() {
        let pcm = voice(60);
        let packets = encode_all(EncoderConfig::default(), &pcm);
        let with_dred = encode_all(dred_only(), &pcm);
        let lost = 40;
        let original = energy(&pcm[lost * FRAME_SAMPLES..(lost + 1) * FRAME_SAMPLES]);

        let fec = with_one_lost(&packets, lost, |decoder, frame| {
            decoder.decode_fec(&packets[lost + 1], frame).unwrap();
        });
        let dred = with_one_lost(&with_dred, lost, |decoder, frame| {
            assert!(decoder.decode_dred(&with_dred[lost + 3], 3, frame).unwrap());
        });
        let concealed = with_one_lost(&packets, lost, |decoder, frame| {
            decoder.conceal(frame).unwrap();
        });
        for (how, frame) in [("fec", &fec), ("dred", &dred), ("concealed", &concealed)] {
            let got = energy(frame);
            assert!(
                got > original * 0.1 && got < original * 4.0,
                "{how}: energy {got}, the original had {original}"
            );
        }
    }

    #[test]
    fn dred_reaches_some_way_back_and_says_when_it_does_not_reach_far_enough() {
        let pcm = voice(80);
        let packets = encode_all(dred_only(), &pcm);
        let mut decoder = Decoder::new().unwrap();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        for packet in &packets[..60] {
            decoder.decode(packet, &mut frame).unwrap();
        }
        // At 32 kbit/s planning for 15% loss the history is about 135 ms: six frames.
        let reach = decoder.dred_reach(&packets[70]).unwrap();
        assert!((4..=10).contains(&reach), "reach of {reach} frames");
        assert!(
            decoder
                .decode_dred(&packets[70], reach, &mut frame)
                .unwrap()
        );
        assert!(
            !decoder
                .decode_dred(&packets[70], reach + 1, &mut frame)
                .unwrap()
        );
        // Nothing to go back to, and further than DRED ever reaches.
        assert!(!decoder.decode_dred(&packets[70], 0, &mut frame).unwrap());
        assert!(
            !decoder
                .decode_dred(&packets[70], MAX_DRED_FRAMES + 1, &mut frame)
                .unwrap()
        );
    }

    #[test]
    fn the_encoder_pays_for_fec_and_dred_out_of_the_bitrate() {
        // This pins what libopus 1.6.1 does, so that a change of version or of defaults
        // does not pass unnoticed. See the table on `EncoderConfig::default`.
        let pcm = voice(120);
        let reach = |config: EncoderConfig| {
            let packets = encode_all(config, &pcm);
            let mut decoder = Decoder::new().unwrap();
            let reaches: Vec<u32> = packets[60..]
                .iter()
                .map(|p| decoder.dred_reach(p).unwrap())
                .collect();
            (
                *reaches.iter().min().unwrap(),
                *reaches.iter().max().unwrap(),
            )
        };
        // The default: FEC, and no room left for DRED.
        assert_eq!(reach(EncoderConfig::default()), (0, 0));
        // DRED alone at the same bitrate: about 135 ms.
        let (low, high) = reach(dred_only());
        assert!(low >= 4 && high <= 10, "{low} to {high} frames");
        // Both, at 64 kbit/s: there is room for some.
        let both_at_64 = EncoderConfig {
            bitrate: 64_000,
            ..EncoderConfig::default()
        };
        assert!(reach(both_at_64).0 >= 4);
        // DRED off: none, whatever the bitrate.
        let off = EncoderConfig {
            bitrate: 64_000,
            dred_frames: 0,
            ..EncoderConfig::default()
        };
        assert_eq!(reach(off), (0, 0));
    }

    #[test]
    fn concealment_fades_to_silence() {
        let pcm = voice(40);
        let packets = encode_all(EncoderConfig::default(), &pcm);
        let mut decoder = Decoder::new().unwrap();
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        for packet in &packets {
            decoder.decode(packet, &mut frame).unwrap();
        }
        let mut energies = vec![];
        for _ in 0..25 {
            decoder.conceal(&mut frame).unwrap();
            energies.push(energy(&frame));
        }
        assert!(energies[0] > 1e-5, "{energies:?}");
        assert!(energies[24] < energies[0] * 0.01, "{energies:?}");
    }

    #[test]
    fn wrong_lengths_and_corrupt_packets_are_errors_not_crashes() {
        let mut encoder = Encoder::new(EncoderConfig::default()).unwrap();
        let mut packet = [0u8; MAX_PACKET_BYTES];
        assert_eq!(
            encoder.encode(&[0.0; 100], &mut packet),
            Err(Error::FrameLength)
        );
        // No room at all for the packet.
        let frame = voice(1);
        assert!(matches!(
            encoder.encode(&frame, &mut packet[..0]),
            Err(Error::Opus(_))
        ));

        let mut decoder = Decoder::new().unwrap();
        let mut pcm = vec![0.0f32; FRAME_SAMPLES];
        assert_eq!(
            decoder.decode(&[1, 2, 3], &mut pcm[..10]),
            Err(Error::FrameLength)
        );
        // Bytes that are not an Opus packet, or not one 20 ms frame.
        let mut state = 0x9e37_79b9u32;
        for length in [0usize, 1, 2, 7, 60, 300] {
            let garbage: Vec<u8> = (0..length)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                })
                .collect();
            // Whatever libopus makes of it, it is an answer and not a crash.
            let _ = decoder.decode(&garbage, &mut pcm);
            let _ = decoder.decode_fec(&garbage, &mut pcm);
            let _ = decoder.decode_dred(&garbage, 3, &mut pcm);
        }
        // The decoder still works afterwards.
        let packets = encode_all(EncoderConfig::default(), &voice(5));
        for p in &packets {
            decoder.decode(p, &mut pcm).unwrap();
        }
    }

    #[test]
    fn encoders_and_decoders_can_be_made_and_dropped_freely() {
        for _ in 0..50 {
            let _encoder = Encoder::new(EncoderConfig::default()).unwrap();
            let _decoder = Decoder::new().unwrap();
        }
        fn is_send<T: Send>() {}
        is_send::<Encoder>();
        is_send::<Decoder>();
    }
}
