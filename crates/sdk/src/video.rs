//! Pictures on a running node: coded frames into groups, and groups back into pictures.
//!
//! Between whatever makes and shows pictures and the engine's [`VideoPort`]:
//!
//! - A [`VideoSender`] takes each picture of this node's camera, codes it, and writes
//!   the frames into groups: a key frame begins a group, and what follows it goes in the
//!   same one. It codes nothing while nobody watches.
//! - A [`VideoReceiver`], one for each track watched, takes a group's bytes as they
//!   arrive and gives back each picture as soon as its frame is whole. When it loses its
//!   place it says so, and waits for a group to begin.
//!
//! Neither waits, spawns or holds the port: the owner passes what each gives to the
//! other side, on whatever thread runs the pictures.
//!
//! [`VideoPort`]: crate::engine::VideoPort

use palaver_khef_core::{GroupObject, GroupReader, GroupWriter};
use palaver_media::picture::Picture;
use palaver_media::video::{Decode, Encode, Error, Kind};

/// The next bytes of a group, to give to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub group: u64,
    /// The first bytes of the group.
    pub start: bool,
    pub bytes: Vec<u8>,
    /// The group is whole.
    pub fin: bool,
}

/// Codes one video track of this node's own into groups.
pub struct VideoSender<E> {
    encoder: E,
    alias: u64,
    wanted: bool,
    /// Someone needs a key frame, and has not had one.
    key_owed: bool,
    /// The fewest frames between two key frames made because they were asked for.
    key_gap: u32,
    /// Frames coded since the last key frame.
    since_key: u32,
    /// The group being written.
    open: Option<(u64, GroupWriter)>,
    next_group: u64,
    coded: Vec<u8>,
}

impl<E: Encode> VideoSender<E> {
    /// A sender for the track the plan calls `alias`.
    ///
    /// Groups are numbered from `first_group`, which must be more than any number this
    /// track has used before, in an earlier run too: the time by the clock, in
    /// milliseconds, will do. A key frame that is asked for is made at once if `key_gap`
    /// frames have been coded since the last, and otherwise when they have.
    pub const fn new(encoder: E, alias: u64, first_group: u64, key_gap: u32) -> Self {
        Self {
            encoder,
            alias,
            wanted: false,
            key_owed: false,
            key_gap,
            since_key: 0,
            open: None,
            next_group: first_group,
            coded: Vec::new(),
        }
    }

    /// Whether anybody watches the track.
    #[must_use]
    pub const fn is_wanted(&self) -> bool {
        self.wanted
    }

    /// Somebody watches the track, or nobody does any more. What begins, begins with a
    /// key frame.
    pub const fn set_wanted(&mut self, wanted: bool) {
        if wanted && !self.wanted {
            self.key_owed = true;
        }
        if !wanted {
            // The group under way was given up along with the watching.
            self.open = None;
        }
        self.wanted = wanted;
    }

    /// Somebody needs a key frame: the next picture, or the first that
    /// [`key_gap`](Self::new) allows.
    pub const fn ask_for_key(&mut self) {
        self.key_owed = true;
    }

    /// Code the next picture and append what there is to send to `out`. Nothing, if
    /// nobody watches or the encoder let the picture go.
    ///
    /// # Errors
    ///
    /// What the encoder says: the picture is the wrong size, or the codec refused.
    pub fn picture(&mut self, picture: &Picture, out: &mut Vec<Piece>) -> Result<(), Error> {
        if !self.wanted {
            return Ok(());
        }
        if self.key_owed && (self.open.is_none() || self.since_key >= self.key_gap) {
            self.encoder.ask_for_key();
            self.key_owed = false;
        }
        let Some(kind) = self.encoder.encode(picture, &mut self.coded)? else {
            return Ok(());
        };
        match kind {
            Kind::Key => {
                if let Some((group, _)) = self.open.take() {
                    out.push(Piece {
                        group,
                        start: false,
                        bytes: Vec::new(),
                        fin: true,
                    });
                }
                let group = self.next_group;
                self.next_group = group.saturating_add(1);
                let mut bytes = Vec::new();
                let mut writer = GroupWriter::begin(self.alias, group, &mut bytes);
                if !writer.object(&self.coded, &mut bytes) {
                    return Err(Error::Codec("a frame too large to send".into()));
                }
                out.push(Piece {
                    group,
                    start: true,
                    bytes,
                    fin: false,
                });
                self.open = Some((group, writer));
                self.since_key = 0;
            }
            Kind::Delta => {
                let Some((group, writer)) = self.open.as_mut() else {
                    // Nothing for it to follow: only a key frame can begin.
                    self.key_owed = true;
                    return Ok(());
                };
                let mut bytes = Vec::new();
                if !writer.object(&self.coded, &mut bytes) {
                    return Err(Error::Codec("a frame too large to send".into()));
                }
                out.push(Piece {
                    group: *group,
                    start: false,
                    bytes,
                    fin: false,
                });
                self.since_key = self.since_key.saturating_add(1);
            }
        }
        Ok(())
    }
}

/// Turns the groups of one watched track back into pictures.
pub struct VideoReceiver<D> {
    decoder: D,
    /// The group being read: the latest that has begun.
    current: Option<u64>,
    reader: GroupReader,
    /// The place in the current group is lost: nothing more of it can be shown.
    lost: bool,
    objects: Vec<GroupObject>,
}

impl<D: Decode> VideoReceiver<D> {
    pub fn new(decoder: D) -> Self {
        Self {
            decoder,
            current: None,
            reader: GroupReader::new(),
            lost: false,
            objects: Vec::new(),
        }
    }

    /// The next bytes of `group`, the first of them if `start`. Every picture now whole
    /// is appended to `out`.
    ///
    /// A group that begins takes the place of an older one still arriving: the newest
    /// picture is the one to show. Returns the group in which the place was lost, if it
    /// just was: ask for a key frame after it ([`VideoPort::ask_key_frame`]).
    ///
    /// [`VideoPort::ask_key_frame`]: crate::engine::VideoPort::ask_key_frame
    pub fn group(
        &mut self,
        group: u64,
        start: bool,
        bytes: &[u8],
        out: &mut Vec<Picture>,
    ) -> Option<u64> {
        if start {
            if self.current.is_some_and(|current| group <= current) {
                return None;
            }
            self.current = Some(group);
            self.reader = GroupReader::new();
            self.lost = false;
        } else if self.current != Some(group) || self.lost {
            return None;
        }
        self.objects.clear();
        if self.reader.feed(bytes, &mut self.objects).is_err() {
            self.lost = true;
            return Some(group);
        }
        for object in self.objects.drain(..) {
            match self.decoder.decode(&object.payload) {
                Ok(Some(picture)) => out.push(picture),
                Ok(None) => {}
                Err(_) => {
                    self.lost = true;
                    return Some(group);
                }
            }
        }
        None
    }

    /// `group` was given up part way. Returns it if that lost the place, as
    /// [`group`](Self::group) does.
    pub fn given_up(&mut self, group: u64) -> Option<u64> {
        if self.current == Some(group) && !self.lost {
            self.lost = true;
            return Some(group);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use palaver_media::picture::Size;

    /// Codes each picture as its number, a key frame first and whenever asked.
    #[derive(Default)]
    struct Counting {
        given: u8,
        key: bool,
        /// Let every picture go.
        skip: bool,
    }

    impl Encode for Counting {
        fn encode(&mut self, _: &Picture, out: &mut Vec<u8>) -> Result<Option<Kind>, Error> {
            if self.skip {
                return Ok(None);
            }
            let asked = core::mem::take(&mut self.key);
            let kind = if self.given == 0 || asked {
                Kind::Key
            } else {
                Kind::Delta
            };
            out.clear();
            out.push(self.given);
            self.given += 1;
            Ok(Some(kind))
        }

        fn ask_for_key(&mut self) {
            self.key = true;
        }
    }

    /// Gives a black picture for every frame but one coded as 13.
    struct Superstitious(Vec<u8>);

    impl Decode for Superstitious {
        fn decode(&mut self, coded: &[u8]) -> Result<Option<Picture>, Error> {
            if coded == [13] {
                return Err(Error::Codec("unlucky".into()));
            }
            self.0.extend_from_slice(coded);
            Ok(Some(picture()))
        }
    }

    fn picture() -> Picture {
        Picture::black(Size::new(16, 16).unwrap())
    }

    fn sender(key_gap: u32) -> VideoSender<Counting> {
        VideoSender::new(Counting::default(), 7, 100, key_gap)
    }

    /// Give the sender `count` pictures; what it made of them as (group, start, fin).
    fn film(sender: &mut VideoSender<Counting>, count: usize) -> Vec<(u64, bool, bool)> {
        let mut out = Vec::new();
        for _ in 0..count {
            sender.picture(&picture(), &mut out).unwrap();
        }
        out.iter().map(|p| (p.group, p.start, p.fin)).collect()
    }

    #[test]
    fn nothing_is_coded_while_nobody_watches() {
        let mut sender = sender(0);
        assert_eq!(film(&mut sender, 3), []);
        assert_eq!(sender.encoder.given, 0);
    }

    #[test]
    fn a_key_frame_begins_a_group_and_ends_the_one_before() {
        let mut sender = sender(0);
        sender.set_wanted(true);
        assert_eq!(
            film(&mut sender, 3),
            [(100, true, false), (100, false, false), (100, false, false)]
        );
        sender.ask_for_key();
        assert_eq!(
            film(&mut sender, 2),
            [(100, false, true), (101, true, false), (101, false, false)]
        );
    }

    #[test]
    fn key_frames_asked_for_come_no_closer_together_than_allowed() {
        let mut sender = sender(3);
        sender.set_wanted(true);
        film(&mut sender, 2);
        sender.ask_for_key();
        sender.ask_for_key();
        // One delta has followed the key frame; two more must before the next.
        assert_eq!(
            film(&mut sender, 3),
            [
                (100, false, false),
                (100, false, false),
                (100, false, true),
                (101, true, false)
            ]
        );
        // And the two requests were one.
        assert_eq!(film(&mut sender, 4).iter().filter(|p| p.1).count(), 0);
    }

    #[test]
    fn watching_that_begins_again_begins_with_a_key_frame_in_a_new_group() {
        let mut sender = sender(100);
        sender.set_wanted(true);
        film(&mut sender, 2);
        sender.set_wanted(false);
        assert_eq!(film(&mut sender, 2), []);
        sender.set_wanted(true);
        assert_eq!(film(&mut sender, 1), [(101, true, false)]);
    }

    #[test]
    fn a_picture_the_encoder_lets_go_sends_nothing() {
        let mut sender = sender(0);
        sender.set_wanted(true);
        sender.encoder.skip = true;
        assert_eq!(film(&mut sender, 2), []);
    }

    /// Three groups from a sender, each a key frame and two deltas, the last left open.
    fn groups() -> Vec<Piece> {
        let mut sender = sender(0);
        sender.set_wanted(true);
        let mut out = Vec::new();
        for i in 0..9 {
            if i % 3 == 0 {
                sender.ask_for_key();
            }
            sender.picture(&picture(), &mut out).unwrap();
        }
        out
    }

    fn receive(receiver: &mut VideoReceiver<Superstitious>, piece: &Piece) -> (usize, Option<u64>) {
        let mut out = Vec::new();
        let lost = receiver.group(piece.group, piece.start, &piece.bytes, &mut out);
        (out.len(), lost)
    }

    #[test]
    fn what_was_sent_is_shown_frame_by_frame_however_the_bytes_are_cut() {
        let mut receiver = VideoReceiver::new(Superstitious(Vec::new()));
        let mut shown = 0;
        for piece in groups() {
            // Byte by byte: a frame is shown when its last byte comes, and not before.
            let mut out = Vec::new();
            for (i, byte) in piece.bytes.iter().enumerate() {
                let lost = receiver.group(piece.group, piece.start && i == 0, &[*byte], &mut out);
                assert_eq!(lost, None);
            }
            shown += out.len();
        }
        assert_eq!(shown, 9);
        assert_eq!(receiver.decoder.0, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_group_that_begins_takes_the_place_of_an_older_one_still_arriving() {
        let mut receiver = VideoReceiver::new(Superstitious(Vec::new()));
        let pieces = groups();
        let of = |group: u64| {
            pieces
                .iter()
                .filter(move |p| p.group == group && !p.bytes.is_empty())
        };
        let (first, second): (Vec<_>, Vec<_>) = (of(100).collect(), of(101).collect());
        assert_eq!(receive(&mut receiver, first[0]), (1, None));
        assert_eq!(receive(&mut receiver, second[0]), (1, None));
        assert_eq!(receive(&mut receiver, first[1]), (0, None), "too late");
        assert_eq!(receive(&mut receiver, second[1]), (1, None));
        // Nor does an old group begin again.
        assert_eq!(receive(&mut receiver, first[0]), (0, None));
        assert_eq!(receiver.decoder.0, [0, 3, 4]);
    }

    #[test]
    fn a_frame_that_cannot_be_decoded_loses_the_place_until_a_group_begins() {
        let mut receiver = VideoReceiver::new(Superstitious(Vec::new()));
        let mut sender = sender(0);
        sender.set_wanted(true);
        sender.encoder.given = 12;
        let mut pieces = Vec::new();
        for _ in 0..3 {
            sender.picture(&picture(), &mut pieces).unwrap();
        }
        sender.ask_for_key();
        sender.picture(&picture(), &mut pieces).unwrap();
        let got: Vec<_> = pieces.iter().map(|p| receive(&mut receiver, p)).collect();
        assert_eq!(
            got,
            [(1, None), (0, Some(100)), (0, None), (0, None), (1, None)],
            "said once, then silence until the next group"
        );
    }

    #[test]
    fn a_group_given_up_part_way_loses_the_place_once() {
        let mut receiver = VideoReceiver::new(Superstitious(Vec::new()));
        let pieces = groups();
        assert_eq!(receive(&mut receiver, &pieces[0]), (1, None));
        assert_eq!(receiver.given_up(99), None, "not the group being read");
        assert_eq!(receiver.given_up(100), Some(100));
        assert_eq!(receiver.given_up(100), None);
        assert_eq!(receive(&mut receiver, &pieces[1]), (0, None));
    }

    #[test]
    fn bytes_that_are_no_group_lose_the_place() {
        let mut receiver = VideoReceiver::new(Superstitious(Vec::new()));
        let mut out = Vec::new();
        assert_eq!(receiver.group(5, true, &[0xff; 64], &mut out), Some(5));
        assert_eq!(out.len(), 0);
    }
}
