//! One group of a track on a stream: writing its frames, and reading them back out.
//!
//! A khef in the middle passes a group's bytes on without looking past the stream's
//! header. The two ends of the line need the frames: the sender writes each as one object
//! ([`GroupWriter`]), and the receiver, given the bytes in whatever pieces the network
//! made of them, takes out each object once it is whole ([`GroupReader`]).

use palaver_moq_wire::{
    Error, ObjectBody, Reader, SubgroupHeader, SubgroupId, SubgroupObject, SubgroupReader,
    SubgroupWriter,
};

/// The largest object a reader will wait for: a frame of video far larger than any we make.
pub const MAX_OBJECT_BYTES: usize = 4 * 1024 * 1024;

/// Writes one group: the stream's header, then an object for each frame.
#[derive(Debug)]
pub struct GroupWriter {
    writer: SubgroupWriter,
    next: u64,
}

impl GroupWriter {
    /// Begin group `group` of the track with `alias`: the stream's header is appended to
    /// `out`.
    #[must_use]
    pub fn begin(alias: u64, group: u64, out: &mut Vec<u8>) -> Self {
        let header = SubgroupHeader {
            track_alias: alias,
            group,
            subgroup: SubgroupId::Zero,
            priority: None,
            has_properties: false,
            end_of_group: true,
            first_object: true,
        };
        header.encode(out);
        Self {
            writer: SubgroupWriter::new(&header),
            next: 0,
        }
    }

    /// Append the next object of the group to `out`. `false`, appending nothing, if it
    /// cannot be written.
    pub fn object(&mut self, payload: &[u8], out: &mut Vec<u8>) -> bool {
        let object = SubgroupObject {
            object: self.next,
            properties: &[],
            body: ObjectBody::Payload(payload),
        };
        let before = out.len();
        if self.writer.write(&object, out).is_err() {
            out.truncate(before);
            return false;
        }
        self.next = self.next.saturating_add(1);
        true
    }
}

/// What a group's bytes held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupObject {
    pub group: u64,
    pub object: u64,
    pub payload: Vec<u8>,
}

/// Why a group could not be read any further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupError {
    /// The bytes are not a group as the draft writes one.
    Malformed,
    /// An object larger than [`MAX_OBJECT_BYTES`].
    TooLarge,
}

/// Reads one group out of its stream's bytes, in whatever pieces they come.
#[derive(Debug, Default)]
pub struct GroupReader {
    buffer: Vec<u8>,
    header: Option<(SubgroupHeader, SubgroupReader)>,
    failed: bool,
}

impl GroupReader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The next bytes of the stream. Every object now whole is appended to `out`.
    ///
    /// # Errors
    ///
    /// When the bytes cannot be a group, or hold an object too large to wait for. The
    /// reader gives nothing further after that.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<GroupObject>) -> Result<(), GroupError> {
        if self.failed {
            return Err(GroupError::Malformed);
        }
        if self.buffer.len().saturating_add(bytes.len()) > MAX_OBJECT_BYTES {
            self.failed = true;
            return Err(GroupError::TooLarge);
        }
        self.buffer.extend_from_slice(bytes);
        let buffer = core::mem::take(&mut self.buffer);
        let mut reader = Reader::new(&buffer);
        if self.header.is_none() {
            match SubgroupHeader::decode(&mut reader) {
                Ok(header) => self.header = Some((header, SubgroupReader::new(&header))),
                Err(Error::NeedMore) => {
                    self.buffer = buffer;
                    return Ok(());
                }
                Err(_) => {
                    self.failed = true;
                    return Err(GroupError::Malformed);
                }
            }
        }
        let Some((header, objects)) = self.header.as_mut() else {
            return Ok(());
        };
        let mut used = buffer.len().saturating_sub(reader.remaining());
        let outcome = loop {
            match objects.read(&mut reader) {
                Ok(object) => {
                    used = buffer.len().saturating_sub(reader.remaining());
                    if let ObjectBody::Payload(payload) = object.body {
                        out.push(GroupObject {
                            group: header.group,
                            object: object.object,
                            payload: payload.to_vec(),
                        });
                    }
                }
                Err(error) => break error,
            }
        };
        self.buffer = buffer.get(used..).unwrap_or_default().to_vec();
        if outcome == Error::NeedMore {
            Ok(())
        } else {
            self.failed = true;
            Err(GroupError::Malformed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(frames: &[&[u8]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut writer = GroupWriter::begin(20_000, 7, &mut bytes);
        for frame in frames {
            assert!(writer.object(frame, &mut bytes));
        }
        bytes
    }

    #[test]
    fn what_is_written_is_read_back_whatever_pieces_it_comes_in() {
        let key = vec![9u8; 3000];
        let frames: [&[u8]; 3] = [&key, b"second", b"3"];
        let bytes = written(&frames);
        for piece in [1, 2, 7, 500, bytes.len()] {
            let mut reader = GroupReader::new();
            let mut out = Vec::new();
            for chunk in bytes.chunks(piece) {
                reader
                    .feed(chunk, &mut out)
                    .unwrap_or_else(|_| unreachable!());
            }
            // Compared a part at a time: a frame is too long to print if they differ.
            let places: Vec<(u64, u64, usize)> = out
                .iter()
                .map(|object| (object.group, object.object, object.payload.len()))
                .collect();
            assert_eq!(
                places,
                [(7, 0, 3000), (7, 1, 6), (7, 2, 1)],
                "pieces of {piece}"
            );
            assert!(
                out.iter()
                    .zip(frames)
                    .all(|(object, frame)| object.payload == frame)
            );
        }
    }

    #[test]
    fn an_object_is_given_only_once_it_is_whole() {
        let bytes = written(&[b"a whole frame of video"]);
        let mut reader = GroupReader::new();
        let mut out = Vec::new();
        let (most, last) = bytes.split_at(bytes.len() - 1);
        assert_eq!(reader.feed(most, &mut out), Ok(()));
        assert!(out.is_empty());
        assert_eq!(reader.feed(last, &mut out), Ok(()));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn what_is_not_a_group_is_refused_and_nothing_follows() {
        let mut reader = GroupReader::new();
        let mut out = Vec::new();
        // A stream that begins as something other than a group.
        assert_eq!(
            reader.feed(&[0x01, 0x00, 0x00], &mut out),
            Err(GroupError::Malformed)
        );
        assert_eq!(
            reader.feed(&written(&[b"x"]), &mut out),
            Err(GroupError::Malformed)
        );
        assert!(out.is_empty());
    }

    #[test]
    fn an_object_too_large_to_wait_for_is_refused() {
        let mut reader = GroupReader::new();
        let mut out = Vec::new();
        let mut bytes = Vec::new();
        let _ = GroupWriter::begin(1, 1, &mut bytes);
        assert_eq!(reader.feed(&bytes, &mut out), Ok(()));
        let flood = vec![0x40u8; MAX_OBJECT_BYTES + 1];
        assert_eq!(reader.feed(&flood, &mut out), Err(GroupError::TooLarge));
    }
}
