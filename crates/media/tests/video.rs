//! Pictures through the software H.264 encoder and decoder, and back.

#![expect(clippy::unwrap_used, reason = "a test may stop where it stands")]

use palaver_media::picture::{Pattern, Picture, Size};
use palaver_media::video::{
    Decode, Encode, Error, Kind, Settings, SoftwareDecoder, SoftwareEncoder,
};

const fn size() -> Size {
    Size::new(320, 180).unwrap()
}

/// How far apart two pictures are: the mean difference of their brightness, of 255.
fn apart(a: &Picture, b: &Picture) -> u64 {
    assert_eq!(a.size(), b.size());
    let sum: u64 = a
        .luma()
        .iter()
        .zip(b.luma())
        .map(|(x, y)| u64::from(x.abs_diff(*y)))
        .sum();
    sum / a.luma().len() as u64
}

/// `count` pictures of a pattern, coded: each with what it was and its bytes.
fn coded(settings: Settings, count: usize) -> Vec<(Picture, Option<Kind>, Vec<u8>)> {
    let mut encoder = SoftwareEncoder::new(settings).unwrap();
    let mut pattern = Pattern::new(settings.size, 2);
    (0..count)
        .map(|_| {
            let picture = pattern.next_picture();
            let mut out = Vec::new();
            let kind = encoder.encode(&picture, &mut out).unwrap();
            (picture, kind, out)
        })
        .collect()
}

#[test]
fn what_goes_in_comes_out_looking_the_same() {
    let frames = coded(Settings::new(size(), 1_000_000), 30);
    assert_eq!(frames[0].1, Some(Kind::Key));
    let mut decoder = SoftwareDecoder::new().unwrap();
    let mut count = 0;
    for (picture, kind, bytes) in &frames {
        if kind.is_none() {
            continue;
        }
        let back = decoder.decode(bytes).unwrap().unwrap();
        assert_eq!(back.size(), size());
        // The bars are 80 apart in brightness; a decoded picture is within a few.
        assert!(apart(picture, &back) < 6, "{}", apart(picture, &back));
        count += 1;
    }
    assert!(count >= 25, "{count} of 30 shown");
    // Coded, a second of it is far smaller than the pictures were.
    let bytes: usize = frames.iter().map(|(_, _, bytes)| bytes.len()).sum();
    assert!(bytes < 30 * size().pixels() / 4, "{bytes} bytes");
}

#[test]
fn a_key_frame_comes_when_asked_for_and_is_where_a_decoder_can_begin() {
    let settings = Settings {
        key_every: None,
        ..Settings::new(size(), 1_000_000)
    };
    let mut encoder = SoftwareEncoder::new(settings).unwrap();
    let mut pattern = Pattern::new(size(), 2);
    let mut frames = Vec::new();
    for n in 0..20 {
        if n == 10 {
            encoder.ask_for_key();
        }
        let mut out = Vec::new();
        let kind = encoder.encode(&pattern.next_picture(), &mut out).unwrap();
        frames.push((kind, out));
    }
    // Only the first, and the one asked for.
    let keys: Vec<usize> = (0..20)
        .filter(|n| frames[*n].0 == Some(Kind::Key))
        .collect();
    assert_eq!(keys, [0, 10]);

    // A decoder that comes in the middle shows nothing until the key frame.
    let mut late = SoftwareDecoder::new().unwrap();
    for (kind, bytes) in &frames[5..10] {
        if kind.is_some() {
            assert!(matches!(
                late.decode(bytes),
                Ok(None) | Err(Error::Codec(_))
            ));
        }
    }
    for (kind, bytes) in &frames[10..] {
        if kind.is_some() {
            assert!(late.decode(bytes).unwrap().is_some());
        }
    }
}

#[test]
fn key_frames_come_by_themselves_as_often_as_set() {
    let settings = Settings {
        key_every: Some(10),
        ..Settings::new(size(), 1_000_000)
    };
    let keys: Vec<usize> = coded(settings, 30)
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.1 == Some(Kind::Key))
        .map(|(n, _)| n)
        .collect();
    assert_eq!(keys, [0, 10, 20]);
}

#[test]
fn a_lower_rate_asked_for_is_a_lower_rate_made() {
    let bytes = |bitrate| -> usize {
        coded(Settings::new(size(), bitrate), 60)
            .iter()
            .map(|(_, _, bytes)| bytes.len())
            .sum()
    };
    let (low, high) = (bytes(150_000), bytes(1_500_000));
    assert!(low < high, "{low} at the low rate, {high} at the high");
}

#[test]
fn sizes_the_codec_does_not_take_are_refused() {
    let odd = Size::new(321, 180).unwrap();
    assert!(matches!(
        SoftwareEncoder::new(Settings::new(odd, 500_000)),
        Err(Error::Size)
    ));
    // And a picture of another size than the encoder was made for.
    let mut encoder = SoftwareEncoder::new(Settings::new(size(), 500_000)).unwrap();
    let other = Picture::black(Size::new(160, 90).unwrap());
    assert_eq!(encoder.encode(&other, &mut Vec::new()), Err(Error::Size));
}
