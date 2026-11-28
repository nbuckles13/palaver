//! One node's pictures, both ways.
//!
//! A [`Pictures`] holds the engine's [`VideoPort`] and stands between it and whatever
//! makes and shows pictures: this node's camera goes out through a [`VideoSender`], and
//! each track it watches comes in through a [`VideoReceiver`]. It does what the engine
//! says on the way: codes nothing while nobody watches, begins a group when someone
//! needs one, and asks for one when it loses its place.
//!
//! It spawns nothing and never waits. Whoever owns it calls [`turn`](Pictures::turn)
//! often and [`film`](Pictures::film) for each picture of the camera, on one thread:
//! the one the codecs were made on.

use crate::engine::{Status, VideoIn, VideoPort};
use crate::video::{Piece, VideoReceiver, VideoSender};
use palaver_media::picture::{Picture, Size};
use palaver_media::video::{Decode, Encode, Error};
use palaver_node::VideoTrack;
use std::collections::{BTreeMap, BTreeSet};
use tokio::sync::watch;

/// The fewest frames between two key frames made because they were asked for: half a
/// second of them.
pub const KEY_GAP: u32 = 15;

/// One node's pictures: its camera out, and what it watches in.
pub struct Pictures<E, D> {
    port: VideoPort,
    status: watch::Receiver<Status>,
    /// A sender for each size of this node's camera, under what the size is called.
    cameras: BTreeMap<Vec<u8>, VideoSender<E>>,
    /// Which of this node's own tracks somebody watches.
    wanted: BTreeSet<u64>,
    watched: BTreeMap<u64, VideoReceiver<D>>,
    pieces: Vec<Piece>,
    shown: Vec<Picture>,
    /// The tracks a key frame has been asked of since they were last taken.
    asked: Vec<u64>,
}

impl<E: Encode, D: Decode> Pictures<E, D> {
    /// Over the engine's door for video and the node's status
    /// ([`Participant::video`](crate::Participant::video)).
    #[must_use]
    pub const fn new(port: VideoPort, status: watch::Receiver<Status>) -> Self {
        Self {
            port,
            status,
            cameras: BTreeMap::new(),
            wanted: BTreeSet::new(),
            watched: BTreeMap::new(),
            pieces: Vec::new(),
            shown: Vec::new(),
            asked: Vec::new(),
        }
    }

    /// Every video track in the plan, this node's own among them.
    #[must_use]
    pub fn tracks(&self) -> Vec<VideoTrack> {
        self.status.borrow().videos.clone()
    }

    /// This node's own video tracks, as the plan lists them.
    fn own(&self) -> Vec<VideoTrack> {
        let status = self.status.borrow();
        status
            .videos
            .iter()
            .filter(|track| track.own)
            .cloned()
            .collect()
    }

    /// The sizes of this node's camera that somebody watches: what to make pictures
    /// for.
    #[must_use]
    pub fn wanted_sizes(&self) -> Vec<VideoTrack> {
        let mut own = self.own();
        own.retain(|track| self.wanted.contains(&track.alias));
        own
    }

    /// Whether somebody watches this node's camera at any size: whether to make
    /// pictures at all.
    #[must_use]
    pub fn wanted(&self) -> bool {
        !self.wanted_sizes().is_empty()
    }

    /// Whether there is an encoder for the size of the camera called `name`.
    #[must_use]
    pub fn has_camera(&self, name: &[u8]) -> bool {
        self.cameras.contains_key(name)
    }

    /// Code the size of the camera called `name` with `encoder`, in place of the one
    /// before if there was one. Groups go on being numbered from where they were.
    pub fn set_camera(&mut self, name: &[u8], encoder: E) {
        let first = self.cameras.get(name).map_or(0, VideoSender::next_group);
        let sender = VideoSender::new(encoder, 0, first, KEY_GAP);
        self.cameras.insert(name.to_vec(), sender);
    }

    /// The next picture of the size of the camera called `name`: coded and sent, if
    /// there is an encoder for it and somebody watches. Returns whether anything was
    /// sent.
    ///
    /// # Errors
    ///
    /// What the encoder says: the picture is the wrong size, or the codec refused.
    pub fn film(&mut self, name: &[u8], picture: &Picture) -> Result<bool, Error> {
        let Some(track) = self.own().into_iter().find(|t| t.rendition == name) else {
            return Ok(false);
        };
        self.film_track(&track, picture)
    }

    fn film_track(&mut self, track: &VideoTrack, picture: &Picture) -> Result<bool, Error> {
        let Some(sender) = self.cameras.get_mut(&track.rendition) else {
            return Ok(false);
        };
        sender.set_alias(track.alias);
        sender.set_wanted(self.wanted.contains(&track.alias));
        let coded = sender.picture(picture, &mut self.pieces);
        let sent = !self.pieces.is_empty();
        for piece in self.pieces.drain(..) {
            self.port.send(
                track.alias,
                piece.group,
                piece.start,
                piece.bytes,
                piece.fin,
            );
        }
        coded.map(|()| sent)
    }

    /// The camera's next picture, at its full size: made as small as each size on offer
    /// that somebody watches says it is, coded and sent. An encoder that is needed and
    /// not there is made by `encoder_for`, which is given the track and the size of the
    /// pictures it will code. Returns how many sizes were sent.
    ///
    /// # Errors
    ///
    /// [`Error::Size`] if a size on offer is not the camera's made a whole number of
    /// times smaller; and what `encoder_for` or an encoder says.
    pub fn film_all(
        &mut self,
        picture: &Picture,
        mut encoder_for: impl FnMut(&VideoTrack, Size) -> Result<E, Error>,
    ) -> Result<usize, Error> {
        let mut smaller: Vec<Picture> = Vec::new();
        let mut sent = 0usize;
        for track in self.own() {
            if !self.wanted.contains(&track.alias) {
                // Told now, so that what begins when it is wanted again begins anew.
                if let Some(sender) = self.cameras.get_mut(&track.rendition) {
                    sender.set_wanted(false);
                }
                continue;
            }
            let full = picture.size();
            let wanted = track
                .shape
                .map_or(Some(full), |shape| Size::new(shape.width, shape.height))
                .ok_or(Error::Size)?;
            let fitting = if wanted == full {
                picture
            } else if let Some(made) = smaller.iter().position(|made| made.size() == wanted) {
                smaller.get(made).ok_or(Error::Size)?
            } else {
                let by = full.width() / wanted.width();
                let made = picture
                    .shrunk(by)
                    .filter(|made| made.size() == wanted)
                    .ok_or(Error::Size)?;
                smaller.push(made);
                smaller.last().ok_or(Error::Size)?
            };
            if !self.has_camera(&track.rendition) {
                let encoder = encoder_for(&track, wanted)?;
                self.set_camera(&track.rendition, encoder);
            }
            if self.film_track(&track, fitting)? {
                sent = sent.saturating_add(1);
            }
        }
        Ok(sent)
    }

    /// Begin to watch the track the plan calls `alias`, decoding it with `decoder`.
    pub fn watch(&mut self, alias: u64, decoder: D) {
        if self
            .watched
            .insert(alias, VideoReceiver::new(decoder))
            .is_none()
        {
            self.port.watch(alias, true);
        }
    }

    /// Stop watching `alias`.
    pub fn unwatch(&mut self, alias: u64) {
        if self.watched.remove(&alias).is_some() {
            self.port.watch(alias, false);
        }
    }

    /// The tracks being watched.
    pub fn watching(&self) -> impl Iterator<Item = u64> + '_ {
        self.watched.keys().copied()
    }

    /// The tracks a key frame has been asked of since this was last called, once for
    /// each asking: for whoever counts them.
    pub fn take_asked(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.asked)
    }

    /// Take everything the engine has to say: `show` is given each picture that is now
    /// whole, with the alias of its track.
    pub fn turn(&mut self, mut show: impl FnMut(u64, Picture)) {
        while let Some(event) = self.port.try_next() {
            match event {
                VideoIn::Produce { alias, wanted } => {
                    if wanted {
                        self.wanted.insert(alias);
                    } else {
                        self.wanted.remove(&alias);
                    }
                    // Whatever it says, what was being made is over: a track wanted
                    // again begins again, even when no picture came in between to
                    // notice that it was not wanted.
                    let sender = self
                        .cameras
                        .values_mut()
                        .find(|sender| sender.alias() == alias);
                    if let Some(sender) = sender {
                        sender.set_wanted(false);
                    }
                }
                VideoIn::KeyFrame { alias } => {
                    let asked = self
                        .cameras
                        .values_mut()
                        .find(|sender| sender.alias() == alias);
                    if let Some(sender) = asked {
                        sender.ask_for_key();
                    }
                }
                VideoIn::Group {
                    alias,
                    group,
                    start,
                    bytes,
                    ..
                } => {
                    let Some(receiver) = self.watched.get_mut(&alias) else {
                        continue;
                    };
                    let lost = receiver.group(group, start, &bytes, &mut self.shown);
                    for picture in self.shown.drain(..) {
                        show(alias, picture);
                    }
                    if let Some(lost) = lost {
                        self.port.ask_key_frame(alias, lost);
                        self.asked.push(alias);
                    }
                }
                VideoIn::Reset { alias, group } => {
                    let lost = self
                        .watched
                        .get_mut(&alias)
                        .and_then(|receiver| receiver.given_up(group));
                    if let Some(lost) = lost {
                        self.port.ask_key_frame(alias, lost);
                        self.asked.push(alias);
                    }
                }
            }
        }
    }
}
