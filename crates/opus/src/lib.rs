//! A safe wrapper over libopus, for one use: speech at 48 kHz in 20 ms frames, in one
//! channel or two.
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
// As on the forwarding path: no arithmetic that can overflow, no indexing that can panic.
#![cfg_attr(
    not(test),
    deny(clippy::arithmetic_side_effects, clippy::indexing_slicing)
)]

use core::ffi::c_int;
use core::ptr::NonNull;
use opusic_sys as sys;

/// The only sample rate used.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples in one 20 ms frame, for each channel.
pub const FRAME_SAMPLES: usize = 960;

/// How many channels a stream of samples has. With two, the samples are interleaved:
/// left, right, left, right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channels {
    #[default]
    Mono,
    Stereo,
}

impl Channels {
    pub fn count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }

    /// How many samples one 20 ms frame holds: [`FRAME_SAMPLES`] for each channel.
    pub fn frame_len(self) -> usize {
        match self {
            Self::Mono => FRAME_SAMPLES,
            Self::Stereo => FRAME_SAMPLES.saturating_mul(2),
        }
    }
}
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
    /// A frame of audio must be exactly [`Channels::frame_len`] long.
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
    /// What the encoder is given. A decoder may ask for either, whatever was sent.
    pub channels: Channels,
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
    /// One channel, 32 kbit/s, DTX and FEC on, DRED off, planning for 15% loss.
    ///
    /// DRED is off to begin with: it is to be turned on when a link shows the bursts of
    /// loss it is for, as part of adapting to the network. FEC and DRED are both paid for
    /// out of the bitrate, and the encoder decides frame by frame what it can afford.
    /// Measured with libopus 1.6.1 on a voice-like test signal, how far back a packet's
    /// DRED history reached when 500 ms was asked for:
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
    /// So turning DRED on at 32 kbit/s means turning FEC off, or raising the bitrate.
    fn default() -> Self {
        Self {
            channels: Channels::Mono,
            bitrate: 32_000,
            dtx: true,
            fec: true,
            dred_frames: 0,
            expected_loss_percent: 15,
        }
    }
}

/// An Opus encoder for speech.
#[derive(Debug)]
pub struct Encoder {
    raw: NonNull<sys::OpusEncoder>,
    channels: Channels,
}

// SAFETY: the encoder state is owned by this value alone and libopus keeps no thread-local
// or global state for it, so it may move to another thread. It is not `Sync`: every method
// that touches the state takes `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self, Error> {
        let mut error: c_int = 0;
        // SAFETY: valid arguments (48 kHz, one or two channels, a defined application) and a valid
        // pointer for the error code. The result is checked for null below.
        let raw = unsafe {
            sys::opus_encoder_create(
                SAMPLE_RATE as i32,
                config.channels.count() as c_int,
                sys::OPUS_APPLICATION_VOIP as c_int,
                &mut error,
            )
        };
        check(error)?;
        let encoder = Self {
            raw: NonNull::new(raw).ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))?,
            channels: config.channels,
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
        if pcm.len() != self.channels.frame_len() {
            return Err(Error::FrameLength);
        }
        let capacity = len(packet.len().min(MAX_PACKET_BYTES))?;
        // SAFETY: `raw` is a live encoder; `pcm` holds exactly the `FRAME_SAMPLES` samples
        // for each of its channels that libopus is told to read; `packet` has at least `capacity` bytes to write into.
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

/// An Opus decoder for speech, with its DRED state.
#[derive(Debug)]
pub struct Decoder {
    channels: Channels,
    raw: NonNull<sys::OpusDecoder>,
    dred_decoder: NonNull<sys::OpusDREDDecoder>,
    dred: NonNull<sys::OpusDRED>,
}

// SAFETY: as for `Encoder`: the three states are owned by this value alone, libopus keeps
// nothing thread-local for them, and every method that touches them takes `&mut self`.
unsafe impl Send for Decoder {}

impl Decoder {
    /// A decoder that gives `channels`, whichever of the two a packet was encoded with:
    /// one channel is given as both, two are averaged into one.
    pub fn new(channels: Channels) -> Result<Self, Error> {
        let mut error: c_int = 0;
        // SAFETY: valid arguments and a valid pointer for the error code; checked for null.
        let raw = unsafe {
            sys::opus_decoder_create(SAMPLE_RATE as i32, channels.count() as c_int, &mut error)
        };
        check(error)?;
        let raw = NonNull::new(raw).ok_or(Error::Opus(sys::OPUS_ALLOC_FAIL))?;
        // From here on, a failure must free what was made: `partial` does that on drop.
        let mut partial = Partial {
            channels,
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

    /// How many channels it gives.
    pub fn channels(&self) -> Channels {
        self.channels
    }

    fn check_output(&self, pcm: &[f32]) -> Result<(), Error> {
        if pcm.len() == self.channels.frame_len() {
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
        self.check_output(pcm)?;
        let (data, length) = match packet {
            Some(packet) => (packet.as_ptr(), len(packet.len())?),
            None => (core::ptr::null(), 0),
        };
        // SAFETY: `raw` is a live decoder; `data` is either null with length 0 (which asks
        // for concealment) or points to `length` readable bytes; `pcm` has room for the
        // `FRAME_SAMPLES` samples for each channel that libopus is told it may write.
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
        self.check_output(pcm)?;
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
        // the `FRAME_SAMPLES` samples for each channel that libopus is told it may write.
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
    channels: Channels,
    raw: Option<NonNull<sys::OpusDecoder>>,
    dred_decoder: Option<NonNull<sys::OpusDREDDecoder>>,
    dred: Option<NonNull<sys::OpusDRED>>,
}

impl Partial {
    fn finish(mut self) -> Option<Decoder> {
        Some(Decoder {
            channels: self.channels,
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
        pcm.chunks_exact(config.channels.frame_len())
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

        let mut decoder = Decoder::new(Channels::Mono).unwrap();
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
        let mut decoder = Decoder::new(Channels::Mono).unwrap();
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

    /// DRED on and FEC off, so that at 32 kbit/s the packets carry DRED.
    fn dred_only() -> EncoderConfig {
        EncoderConfig {
            fec: false,
            dred_frames: 25,
            ..EncoderConfig::default()
        }
    }

    /// FEC and DRED both asked for.
    fn both() -> EncoderConfig {
        EncoderConfig {
            dred_frames: 25,
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
        let mut decoder = Decoder::new(Channels::Mono).unwrap();
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
            let mut decoder = Decoder::new(Channels::Mono).unwrap();
            let reaches: Vec<u32> = packets[60..]
                .iter()
                .map(|p| decoder.dred_reach(p).unwrap())
                .collect();
            (
                *reaches.iter().min().unwrap(),
                *reaches.iter().max().unwrap(),
            )
        };
        // The default has DRED off.
        assert_eq!(reach(EncoderConfig::default()), (0, 0));
        // Asking for both at 32 kbit/s: FEC, and no room left for DRED.
        assert_eq!(reach(both()), (0, 0));
        // DRED alone at the same bitrate: about 135 ms.
        let (low, high) = reach(dred_only());
        assert!(low >= 4 && high <= 10, "{low} to {high} frames");
        // Both, at 64 kbit/s: there is room for some.
        let both_at_64 = EncoderConfig {
            bitrate: 64_000,
            ..both()
        };
        assert!(reach(both_at_64).0 >= 4);
        // DRED off: none, whatever the bitrate.
        let off = EncoderConfig {
            bitrate: 64_000,
            ..EncoderConfig::default()
        };
        assert_eq!(reach(off), (0, 0));
    }

    #[test]
    fn concealment_fades_to_silence() {
        let pcm = voice(40);
        let packets = encode_all(EncoderConfig::default(), &pcm);
        let mut decoder = Decoder::new(Channels::Mono).unwrap();
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

        let mut decoder = Decoder::new(Channels::Mono).unwrap();
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

    /// Two voices at once, interleaved: `voice` on the left, and on the right the same
    /// an octave and a bit higher and half a second out of step.
    fn two_voices(frames: usize) -> Vec<f32> {
        let left = voice(frames);
        (0..left.len())
            .flat_map(|n| {
                let t = n as f32 / SAMPLE_RATE as f32;
                let envelope = 0.5 + 0.5 * (2.0 * core::f32::consts::PI * 3.0 * (t + 0.5)).sin();
                let right = 0.25 * envelope * (2.0 * core::f32::consts::PI * 330.0 * t).sin();
                [left[n], right]
            })
            .collect()
    }

    fn stereo(config: EncoderConfig) -> EncoderConfig {
        EncoderConfig {
            channels: Channels::Stereo,
            bitrate: 64_000,
            ..config
        }
    }

    /// How alike two signals are: 1 for the same shape, 0 for unrelated.
    fn likeness(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(a, b)| a * b).sum();
        let (a, b) = (energy(a) * a.len() as f32, energy(b) * b.len() as f32);
        dot / (a * b).sqrt()
    }

    fn side(interleaved: &[f32], which: usize) -> Vec<f32> {
        interleaved.iter().skip(which).step_by(2).copied().collect()
    }

    #[test]
    fn two_channels_stay_two_channels() {
        let pcm = two_voices(100);
        let packets = encode_all(stereo(EncoderConfig::default()), &pcm);
        let mut decoder = Decoder::new(Channels::Stereo).unwrap();
        assert_eq!(decoder.channels(), Channels::Stereo);
        let mut out = vec![0.0f32; pcm.len()];
        let frame = Channels::Stereo.frame_len();
        assert_eq!(frame, 2 * FRAME_SAMPLES);
        for (packet, frame) in packets.iter().zip(out.chunks_exact_mut(frame)) {
            decoder.decode(packet, frame).unwrap();
        }
        // Each side comes out with about the energy that went in on that side, and the
        // two are no more alike than they were: nothing has run from one into the other.
        let settled = 20 * frame;
        for which in [0, 1] {
            let (sent, got) = (side(&pcm[settled..], which), side(&out[settled..], which));
            let ratio = energy(&got) / energy(&sent);
            assert!((0.5..=1.5).contains(&ratio), "side {which}: {ratio}");
        }
        let alike = likeness(&side(&out[settled..], 0), &side(&out[settled..], 1));
        assert!(alike.abs() < 0.2, "the sides are {alike} alike");
    }

    #[test]
    fn a_decoder_gives_the_channels_it_was_asked_for_whatever_was_sent() {
        // One channel sent, two asked for: the same on both sides.
        let pcm = voice(50);
        let packets = encode_all(EncoderConfig::default(), &pcm);
        let mut decoder = Decoder::new(Channels::Stereo).unwrap();
        let mut frame = vec![0.0f32; Channels::Stereo.frame_len()];
        for packet in &packets {
            decoder.decode(packet, &mut frame).unwrap();
        }
        assert!(energy(&frame) > 1e-4);
        assert!(frame.chunks_exact(2).all(|pair| pair[0] == pair[1]));

        // Two sent, one asked for: the two averaged.
        let pcm = two_voices(50);
        let packets = encode_all(stereo(EncoderConfig::default()), &pcm);
        let mut decoder = Decoder::new(Channels::Mono).unwrap();
        let mut out = vec![0.0f32; 50 * FRAME_SAMPLES];
        for (packet, frame) in packets.iter().zip(out.chunks_exact_mut(FRAME_SAMPLES)) {
            decoder.decode(packet, frame).unwrap();
        }
        let averaged: Vec<f32> = pcm
            .chunks_exact(2)
            .map(|pair| (pair[0] + pair[1]) / 2.0)
            .collect();
        let settled = 20 * FRAME_SAMPLES;
        let ratio = energy(&out[settled..]) / energy(&averaged[settled..]);
        assert!((0.5..=1.5).contains(&ratio), "{ratio}");
    }

    #[test]
    fn a_lost_frame_of_two_channels_is_filled_all_three_ways() {
        let pcm = two_voices(60);
        let frame_len = Channels::Stereo.frame_len();
        let lost = 40;
        let original = energy(&pcm[lost * frame_len..(lost + 1) * frame_len]);
        let run = |packets: &[Vec<u8>], fill: &dyn Fn(&mut Decoder, &mut [f32])| {
            let mut decoder = Decoder::new(Channels::Stereo).unwrap();
            let mut frame = vec![0.0f32; frame_len];
            for packet in &packets[..lost] {
                decoder.decode(packet, &mut frame).unwrap();
            }
            fill(&mut decoder, &mut frame);
            energy(&frame)
        };
        let packets = encode_all(stereo(EncoderConfig::default()), &pcm);
        let with_dred = encode_all(stereo(dred_only()), &pcm);
        let fec = run(&packets, &|decoder, frame| {
            decoder.decode_fec(&packets[lost + 1], frame).unwrap();
        });
        let dred = run(&with_dred, &|decoder, frame| {
            assert!(decoder.dred_reach(&with_dred[lost + 3]).unwrap() >= 3);
            assert!(decoder.decode_dred(&with_dred[lost + 3], 3, frame).unwrap());
        });
        let concealed = run(&packets, &|decoder, frame| {
            decoder.conceal(frame).unwrap();
        });
        for (how, got) in [("fec", fec), ("dred", dred), ("concealed", concealed)] {
            assert!(
                got > original * 0.1 && got < original * 4.0,
                "{how}: energy {got}, the original had {original}"
            );
        }
    }

    #[test]
    fn a_frame_must_be_as_long_as_its_channels_make_it() {
        let mut packet = [0u8; MAX_PACKET_BYTES];
        let mut two = Encoder::new(stereo(EncoderConfig::default())).unwrap();
        assert_eq!(
            two.encode(&[0.0; FRAME_SAMPLES], &mut packet),
            Err(Error::FrameLength)
        );
        assert!(two.encode(&[0.0; 2 * FRAME_SAMPLES], &mut packet).is_ok());
        let mut one = Encoder::new(EncoderConfig::default()).unwrap();
        assert_eq!(
            one.encode(&[0.0; 2 * FRAME_SAMPLES], &mut packet),
            Err(Error::FrameLength)
        );
        let mut decoder = Decoder::new(Channels::Stereo).unwrap();
        assert_eq!(
            decoder.conceal(&mut [0.0; FRAME_SAMPLES]),
            Err(Error::FrameLength)
        );
        assert_eq!(Channels::default(), Channels::Mono);
        assert_eq!((Channels::Mono.count(), Channels::Stereo.count()), (1, 2));
    }

    #[test]
    fn encoders_and_decoders_can_be_made_and_dropped_freely() {
        for _ in 0..50 {
            let _encoder = Encoder::new(EncoderConfig::default()).unwrap();
            let _decoder = Decoder::new(Channels::Mono).unwrap();
        }
        fn is_send<T: Send>() {}
        is_send::<Encoder>();
        is_send::<Decoder>();
    }
}
