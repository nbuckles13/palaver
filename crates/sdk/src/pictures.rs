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
use palaver_media::picture::Picture;
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
    camera: Option<VideoSender<E>>,
    /// Which of this node's own tracks somebody watches.
    wanted: BTreeSet<u64>,
    watched: BTreeMap<u64, VideoReceiver<D>>,
    pieces: Vec<Piece>,
    shown: Vec<Picture>,
}

impl<E: Encode, D: Decode> Pictures<E, D> {
    /// Over the engine's door for video and the node's status
    /// ([`Participant::video`](crate::Participant::video)).
    #[must_use]
    pub const fn new(port: VideoPort, status: watch::Receiver<Status>) -> Self {
        Self {
            port,
            status,
            camera: None,
            wanted: BTreeSet::new(),
            watched: BTreeMap::new(),
            pieces: Vec::new(),
            shown: Vec::new(),
        }
    }

    /// Every video track in the plan, this node's own among them.
    #[must_use]
    pub fn tracks(&self) -> Vec<VideoTrack> {
        self.status.borrow().videos.clone()
    }

    /// What the plan calls this node's camera, if it offered one and the plan has it.
    fn own(&self) -> Option<u64> {
        let status = self.status.borrow();
        status
            .videos
            .iter()
            .find(|track| track.own)
            .map(|track| track.alias)
    }

    /// Whether somebody watches this node's camera: whether to make pictures at all.
    #[must_use]
    pub fn wanted(&self) -> bool {
        self.own().is_some_and(|alias| self.wanted.contains(&alias))
    }

    /// Whether there is an encoder for the camera.
    #[must_use]
    pub const fn has_camera(&self) -> bool {
        self.camera.is_some()
    }

    /// Code the camera's pictures with `encoder`, in place of the one before if there
    /// was one. Groups go on being numbered from where they were.
    pub fn set_camera(&mut self, encoder: E) {
        let first = self.camera.as_ref().map_or(0, VideoSender::next_group);
        self.camera = Some(VideoSender::new(encoder, 0, first, KEY_GAP));
    }

    /// The camera's next picture: coded and sent, if there is an encoder and somebody
    /// watches. Returns whether anything was sent.
    ///
    /// # Errors
    ///
    /// What the encoder says: the picture is the wrong size, or the codec refused.
    pub fn film(&mut self, picture: &Picture) -> Result<bool, Error> {
        let (Some(alias), Some(sender)) = (self.own(), self.camera.as_mut()) else {
            return Ok(false);
        };
        sender.set_alias(alias);
        sender.set_wanted(self.wanted.contains(&alias));
        let coded = sender.picture(picture, &mut self.pieces);
        let sent = !self.pieces.is_empty();
        for piece in self.pieces.drain(..) {
            self.port
                .send(alias, piece.group, piece.start, piece.bytes, piece.fin);
        }
        coded.map(|()| sent)
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
                }
                VideoIn::KeyFrame { .. } => {
                    if let Some(sender) = self.camera.as_mut() {
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
                    }
                }
                VideoIn::Reset { alias, group } => {
                    let lost = self
                        .watched
                        .get_mut(&alias)
                        .and_then(|receiver| receiver.given_up(group));
                    if let Some(lost) = lost {
                        self.port.ask_key_frame(alias, lost);
                    }
                }
            }
        }
    }
}
