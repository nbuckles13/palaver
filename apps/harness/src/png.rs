//! Writing a picture as a PNG file, for pictures of the window.
//!
//! Only what that needs: eight bits each of red, green, blue and alpha, and no
//! compression (the pixels are stored as they are, inside the wrapping the format asks
//! for). A picture of the window is a megabyte or two this way.

/// The checksum PNG puts on each of its chunks.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let low = crc & 1;
            crc >>= 1;
            if low == 1 {
                crc ^= 0xEDB8_8320;
            }
        }
    }
    !crc
}

/// The checksum zlib puts on what it wraps.
fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: [u8; 4], body: &[u8]) {
    let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    let mut checked = kind.to_vec();
    checked.extend_from_slice(body);
    out.extend_from_slice(&checked);
    out.extend_from_slice(&crc32(&checked).to_be_bytes());
}

/// A picture `width` by `height`, its pixels row by row as red, green, blue, alpha.
/// `None` if `rgba` is not that many pixels, or the picture is too large for the format.
#[must_use]
pub fn encode(width: usize, height: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let row = width.checked_mul(4)?;
    if row == 0 || rgba.len() != row.checked_mul(height)? {
        return None;
    }
    // Each row is given a leading byte saying it is stored unfiltered.
    let mut raw = Vec::with_capacity(rgba.len().saturating_add(height));
    for line in rgba.chunks_exact(row) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    // zlib: a header, the bytes in stored blocks of at most 65,535, a checksum.
    let mut packed = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        let length = u16::try_from(block.len()).ok()?;
        packed.push(u8::from(blocks.peek().is_none()));
        packed.extend_from_slice(&length.to_le_bytes());
        packed.extend_from_slice(&(!length).to_le_bytes());
        packed.extend_from_slice(block);
    }
    packed.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&u32::try_from(width).ok()?.to_be_bytes());
    header.extend_from_slice(&u32::try_from(height).ok()?.to_be_bytes());
    // Eight bits a channel, colour with alpha, and the format's only choices for the rest.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    chunk(&mut out, *b"IHDR", &header);
    chunk(&mut out, *b"IDAT", &packed);
    chunk(&mut out, *b"IEND", &[]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksums_are_the_standard_ones() {
        // The values every implementation gives for these inputs.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn a_small_picture_has_the_parts_a_reader_expects() {
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255];
        let png = encode(2, 1, &pixels).unwrap();
        assert_eq!(png[..8], [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        // The header chunk: 13 bytes, two wide and one high.
        assert_eq!(png[8..16], [0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        assert_eq!(png[16..24], [0, 0, 0, 2, 0, 0, 0, 1]);
        // The pixels are in it as they were given, after the row's leading byte.
        let stored: Vec<u8> = [vec![0], pixels.to_vec()].concat();
        assert!(png.windows(stored.len()).any(|part| part == stored));
        assert_eq!(
            png[png.len() - 12..],
            [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]
        );
    }

    #[test]
    fn a_picture_of_more_than_one_block_ends_its_blocks_once() {
        // 200 by 100 is 80,100 bytes with the leading bytes: two stored blocks.
        let png = encode(200, 100, &vec![7; 200 * 100 * 4]).unwrap();
        assert!(png.len() > 80_100);
        assert!(encode(2, 2, &[0; 15]).is_none());
        assert!(encode(0, 1, &[]).is_none());
    }
}
