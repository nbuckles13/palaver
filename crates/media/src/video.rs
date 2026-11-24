//! Video coding: pictures into H.264 and back.
//!
//! [`Encode`] and [`Decode`] are our own interface, so that what does the work can
//! change: [`SoftwareEncoder`] and [`SoftwareDecoder`] (openh264, on the processor) are
//! the first, and the hardware ones of each system come behind the same two traits.
//!
//! What an encoder gives for a picture is a whole coded frame in the byte-stream form
//! (start codes before each unit), which is what a decoder takes. A [`Kind::Key`] frame
//! can be decoded with nothing before it; a decoder that starts anywhere else gives
//! nothing until one comes.

use crate::picture::{Picture, Size};
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::YUVSource;
use openh264::{OpenH264API, Timestamp};

/// Why a picture could not be coded, or a frame decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The picture is not the size the encoder was made for, or its size is odd either
    /// way, which this codec does not take.
    Size,
    /// The codec refused, in its own words.
    Codec(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Size => f.write_str("a picture of a size the codec does not take"),
            Self::Codec(why) => write!(f, "the video codec refused: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// What a coded frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Decodable with nothing before it: where a group begins.
    Key,
    /// Needs the frames since the last key frame.
    Delta,
}

/// What an encoder is made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub size: Size,
    /// The rate to aim for, in bits a second.
    pub bitrate: u32,
    /// How many pictures a second it will be given.
    pub per_second: u32,
    /// A key frame by itself this often, in pictures; `None` for only when asked.
    pub key_every: Option<u32>,
}

impl Settings {
    /// Thirty pictures a second, a key frame every two seconds.
    #[must_use]
    pub const fn new(size: Size, bitrate: u32) -> Self {
        Self {
            size,
            bitrate,
            per_second: 30,
            key_every: Some(60),
        }
    }
}

/// Turns pictures into coded frames.
pub trait Encode {
    /// Code one picture into `out`, which is emptied first. `None` if the encoder let
    /// this picture go to hold its rate: nothing is to be sent for it.
    ///
    /// # Errors
    ///
    /// [`Error::Size`] for a picture of another size than the encoder was made for;
    /// [`Error::Codec`] if the codec refuses.
    fn encode(&mut self, picture: &Picture, out: &mut Vec<u8>) -> Result<Option<Kind>, Error>;

    /// Make the next frame a key frame: someone has begun to watch, or lost their place.
    fn ask_for_key(&mut self);
}

/// Turns coded frames back into pictures.
pub trait Decode {
    /// Decode one coded frame. `None` if there is no picture for it yet: nothing can be
    /// shown before the first key frame.
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] if what it was given cannot be decoded.
    fn decode(&mut self, coded: &[u8]) -> Result<Option<Picture>, Error>;
}

/// A picture as the codec reads one.
struct Planes<'a>(&'a Picture);

impl YUVSource for Planes<'_> {
    fn dimensions(&self) -> (usize, usize) {
        let size = self.0.size();
        (size.width() as usize, size.height() as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        let size = self.0.size();
        let (across, _) = size.colour();
        (size.width() as usize, across, across)
    }

    fn y(&self) -> &[u8] {
        self.0.luma()
    }

    fn u(&self) -> &[u8] {
        self.0.colour().0
    }

    fn v(&self) -> &[u8] {
        self.0.colour().1
    }
}

/// H.264 on the processor.
pub struct SoftwareEncoder {
    inner: Encoder,
    size: Size,
    per_second: u32,
    /// How many pictures it has been given: each is told its time by its place.
    given: u64,
}

impl SoftwareEncoder {
    /// # Errors
    ///
    /// [`Error::Size`] if the size is odd either way; [`Error::Codec`] if the codec
    /// will not start with these settings.
    #[expect(
        clippy::cast_precision_loss,
        reason = "pictures a second: a small whole number, as the codec wants it"
    )]
    pub fn new(settings: Settings) -> Result<Self, Error> {
        let size = settings.size;
        if !size.width().is_multiple_of(2) || !size.height().is_multiple_of(2) {
            return Err(Error::Size);
        }
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(settings.bitrate))
            .max_frame_rate(FrameRate::from_hz(settings.per_second.max(1) as f32))
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            // It may let a picture go to hold its rate; the rest of the way allows it.
            .skip_frames(true)
            .intra_frame_period(
                settings
                    .key_every
                    .map_or_else(IntraFramePeriod::auto, IntraFramePeriod::from_num_frames),
            );
        let inner = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|error| Error::Codec(error.to_string()))?;
        Ok(Self {
            inner,
            size,
            per_second: settings.per_second.max(1),
            given: 0,
        })
    }
}

impl Encode for SoftwareEncoder {
    fn encode(&mut self, picture: &Picture, out: &mut Vec<u8>) -> Result<Option<Kind>, Error> {
        out.clear();
        if picture.size() != self.size {
            return Err(Error::Size);
        }
        // The codec holds its rate by the pictures' times, in milliseconds.
        let at = self.given.saturating_mul(1000) / u64::from(self.per_second);
        self.given = self.given.saturating_add(1);
        let coded = self
            .inner
            .encode_at(&Planes(picture), Timestamp::from_millis(at))
            .map_err(|error| Error::Codec(error.to_string()))?;
        let kind = match coded.frame_type() {
            FrameType::IDR | FrameType::I => Kind::Key,
            FrameType::P | FrameType::IPMixed => Kind::Delta,
            FrameType::Skip | FrameType::Invalid => return Ok(None),
        };
        coded.write_vec(out);
        Ok((!out.is_empty()).then_some(kind))
    }

    fn ask_for_key(&mut self) {
        self.inner.force_intra_frame();
    }
}

/// H.264 on the processor.
pub struct SoftwareDecoder {
    inner: Decoder,
}

impl SoftwareDecoder {
    /// # Errors
    ///
    /// [`Error::Codec`] if the codec will not start.
    pub fn new() -> Result<Self, Error> {
        let inner = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(|error| Error::Codec(error.to_string()))?;
        Ok(Self { inner })
    }
}

/// Copy `rows` rows of `width` values out of a plane whose rows are `stride` apart.
fn copy_plane(from: &[u8], stride: usize, to: &mut [u8], width: usize) {
    for (row, line) in to.chunks_exact_mut(width.max(1)).enumerate() {
        let start = row.saturating_mul(stride);
        if let Some(source) = from.get(start..start.saturating_add(width)) {
            line.copy_from_slice(source);
        }
    }
}

impl Decode for SoftwareDecoder {
    fn decode(&mut self, coded: &[u8]) -> Result<Option<Picture>, Error> {
        let decoded = self
            .inner
            .decode(coded)
            .map_err(|error| Error::Codec(error.to_string()))?;
        let Some(decoded) = decoded else {
            return Ok(None);
        };
        let (width, height) = decoded.dimensions();
        let size = u32::try_from(width)
            .ok()
            .zip(u32::try_from(height).ok())
            .and_then(|(width, height)| Size::new(width, height))
            .ok_or(Error::Size)?;
        let mut picture = Picture::black(size);
        let (luma_stride, colour_stride, _) = decoded.strides();
        let (across, _) = size.colour();
        let planes = picture.planes();
        copy_plane(decoded.y(), luma_stride, planes.luma, width);
        copy_plane(decoded.u(), colour_stride, planes.blue, across);
        copy_plane(decoded.v(), colour_stride, planes.red, across);
        Ok(Some(picture))
    }
}
