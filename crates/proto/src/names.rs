//! Track naming (design section 4).
//!
//! A `MoQ` track is a namespace tuple plus a name. `MoQ` has no wildcards, only namespace-prefix
//! matching, so the fields that need wildcarding go last:
//!
//! ```text
//! namespace = ( meeting, kind,  source, participant )    name = rendition
//!             ( m-7f3a , audio, main  , p12 )          /  opus
//!             ( m-7f3a , video, main  , p12 )          /  1080p | 360p | 180p
//!             ( m-7f3a , video, screen, p12 )          /  1080p
//!             ( m-7f3a , ctl  , report, p12 )          /  0
//!             ( m-7f3a , ctl  , plan )                 /  0
//! ```
//!
//! Nothing human-readable is on the wire: `meeting` is an opaque value derived from the
//! meeting secret, and `participant` is a short node id.

use crate::Redacted;
use crate::fields::{DecodeError, Writer, fields};
use palaver_moq_wire::{FullTrackName, Namespace, Value};

const AUDIO: &[u8] = b"audio";
const VIDEO: &[u8] = b"video";
const CTL: &[u8] = b"ctl";
const REPORT: &[u8] = b"report";
const PLAN: &[u8] = b"plan";
const MAIN: &[u8] = b"main";
const SCREEN: &[u8] = b"screen";
/// The name of both control tracks.
const CONTROL_NAME: &[u8] = b"0";

/// Longest meeting tag, participant id or rendition, in bytes.
pub const MAX_FIELD_BYTES: usize = 64;

fn field(bytes: &[u8]) -> Option<Vec<u8>> {
    (!bytes.is_empty() && bytes.len() <= MAX_FIELD_BYTES).then(|| bytes.to_vec())
}

/// The opaque value that identifies a meeting on the wire.
///
/// It is derived from the meeting secret (the derivation lives with admission, not here), so
/// every member computes the same tag and it means nothing to anyone outside the meeting.
/// Redacted in logs: it would let two log lines be tied to the same meeting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MeetingTag(Redacted<Vec<u8>>);

impl MeetingTag {
    /// `None` if `bytes` is empty or longer than [`MAX_FIELD_BYTES`].
    #[must_use]
    pub fn new(bytes: &[u8]) -> Option<Self> {
        field(bytes).map(|b| Self(Redacted::new(b)))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.reveal()
    }
}

/// A node's short id within one meeting: the first 16 bytes of a hash of its per-meeting
/// public key (computed where the key lives, not here), so an id can be checked against the
/// key that signs for it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ParticipantId(Vec<u8>);

impl ParticipantId {
    /// `None` if `bytes` is empty or longer than [`MAX_FIELD_BYTES`].
    pub fn new(bytes: &[u8]) -> Option<Self> {
        field(bytes).map(Self)
    }

    /// The id for a node whose public key hashes to `hash` (the first 16 bytes of the hash).
    #[must_use]
    pub fn from_key_hash(hash: [u8; 16]) -> Self {
        Self(hash.to_vec())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Which of a participant's media a track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Microphone or camera.
    Main,
    /// A shared screen, and its audio.
    Screen,
}

impl Source {
    const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Main => MAIN,
            Self::Screen => SCREEN,
        }
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        match bytes {
            MAIN => Some(Self::Main),
            SCREEN => Some(Self::Screen),
            _ => None,
        }
    }
}

/// A track within a meeting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Track {
    Audio {
        source: Source,
        participant: ParticipantId,
        rendition: Vec<u8>,
    },
    Video {
        source: Source,
        participant: ParticipantId,
        rendition: Vec<u8>,
    },
    /// A node's signed report.
    Report { participant: ParticipantId },
    /// The conductor's signed plan.
    Plan,
}

/// The alias of the plan track.
pub const PLAN_ALIAS: u64 = 1 << 61;
/// Set in the alias of every report track.
const REPORT_ALIAS: u64 = PLAN_ALIAS | 1 << 60;

// Field numbers of a media track inside a report or a plan.
const MEDIA_RENDITION: u64 = 1;
const MEDIA_KIND: u64 = 2;
const MEDIA_SOURCE: u64 = 4;
const MEDIA_ALIAS: u64 = 6;
const MEDIA_WIDTH: u64 = 8;
const MEDIA_HEIGHT: u64 = 10;
const MEDIA_PER_SECOND: u64 = 12;
const MEDIA_BITRATE: u64 = 14;

/// What a video track's pictures are, as whoever makes them says.
///
/// Whoever watches chooses among a participant's tracks by this, and never by what a
/// track is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Shape {
    /// In pixels.
    pub width: u32,
    pub height: u32,
    /// Pictures a second.
    pub per_second: u32,
    /// The rate it is coded at, in bits a second.
    pub bitrate: u32,
}

/// One of a participant's media tracks, without saying whose: what a node lists in its
/// report as the tracks it can send, and what the plan gives an alias to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MediaTrack {
    /// Audio, as opposed to video. Audio packets are small, so their alias should be too.
    pub audio: bool,
    pub source: Source,
    /// What the track is called among its participant's: a label, with no meaning.
    pub rendition: Vec<u8>,
    /// What a video track's pictures are. Whoever offers the track may change it and
    /// the track stays the same one: see [`same_name`](Self::same_name).
    pub shape: Option<Shape>,
}

impl MediaTrack {
    /// Whether this is the track `other` is, whatever each says of its shape: a track
    /// is the one its name says.
    #[must_use]
    pub fn same_name(&self, other: &Self) -> bool {
        (self.audio, self.source, &self.rendition) == (other.audio, other.source, &other.rendition)
    }

    /// This track as `participant`'s.
    #[must_use]
    pub fn of(&self, participant: ParticipantId) -> Track {
        let (source, rendition) = (self.source, self.rendition.clone());
        if self.audio {
            Track::Audio {
                source,
                participant,
                rendition,
            }
        } else {
            Track::Video {
                source,
                participant,
                rendition,
            }
        }
    }

    /// Encoded as a nested structure, with its alias if it has been given one.
    pub(crate) fn encode(&self, alias: Option<u64>) -> Vec<u8> {
        let mut w = Writer::default();
        w.bytes(MEDIA_RENDITION, &self.rendition);
        w.int(MEDIA_KIND, u64::from(!self.audio));
        w.int(MEDIA_SOURCE, u64::from(self.source == Source::Screen));
        if let Some(alias) = alias {
            w.int(MEDIA_ALIAS, alias);
        }
        if let Some(shape) = self.shape {
            w.int(MEDIA_WIDTH, u64::from(shape.width));
            w.int(MEDIA_HEIGHT, u64::from(shape.height));
            w.int(MEDIA_PER_SECOND, u64::from(shape.per_second));
            w.int(MEDIA_BITRATE, u64::from(shape.bitrate));
        }
        w.finish()
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<(Self, Option<u64>), DecodeError> {
        let (mut rendition, mut audio, mut source, mut alias) = (None, None, None, None);
        let mut shape: [Option<u32>; 4] = [None; 4];
        let small = |v: u64| u32::try_from(v).map_err(|_| DecodeError::Invalid("track shape"));
        for pair in fields(bytes) {
            let pair = pair?;
            match (pair.ty, pair.value) {
                (MEDIA_RENDITION, Value::Bytes(b)) => rendition = field(b),
                (MEDIA_KIND, Value::Int(0)) => audio = Some(true),
                (MEDIA_KIND, Value::Int(1)) => audio = Some(false),
                (MEDIA_KIND, _) => return Err(DecodeError::Invalid("track kind")),
                (MEDIA_SOURCE, Value::Int(0)) => source = Some(Source::Main),
                (MEDIA_SOURCE, Value::Int(1)) => source = Some(Source::Screen),
                (MEDIA_SOURCE, _) => return Err(DecodeError::Invalid("track source")),
                (MEDIA_ALIAS, Value::Int(v)) => alias = Some(v),
                (MEDIA_WIDTH, Value::Int(v)) => shape[0] = Some(small(v)?),
                (MEDIA_HEIGHT, Value::Int(v)) => shape[1] = Some(small(v)?),
                (MEDIA_PER_SECOND, Value::Int(v)) => shape[2] = Some(small(v)?),
                (MEDIA_BITRATE, Value::Int(v)) => shape[3] = Some(small(v)?),
                _ => {}
            }
        }
        let track = Self {
            audio: audio.ok_or(DecodeError::Missing("track kind"))?,
            source: source.ok_or(DecodeError::Missing("track source"))?,
            rendition: rendition.ok_or(DecodeError::Invalid("track rendition"))?,
            // All of a shape or none of it.
            shape: match shape {
                [Some(width), Some(height), Some(per_second), Some(bitrate)] => Some(Shape {
                    width,
                    height,
                    per_second,
                    bitrate,
                }),
                [None, None, None, None] => None,
                _ => return Err(DecodeError::Invalid("track shape")),
            },
        };
        Ok((track, alias))
    }
}

impl Track {
    /// The participant whose track this is; `None` for the plan.
    #[must_use]
    pub const fn participant(&self) -> Option<&ParticipantId> {
        match self {
            Self::Audio { participant, .. }
            | Self::Video { participant, .. }
            | Self::Report { participant } => Some(participant),
            Self::Plan => None,
        }
    }

    /// This track without its participant, if it is a media track. A name says nothing
    /// of a shape.
    #[must_use]
    pub fn media(&self) -> Option<MediaTrack> {
        let (audio, source, rendition) = match self {
            Self::Audio {
                source, rendition, ..
            } => (true, *source, rendition),
            Self::Video {
                source, rendition, ..
            } => (false, *source, rendition),
            _ => return None,
        };
        Some(MediaTrack {
            audio,
            source,
            rendition: rendition.clone(),
            shape: None,
        })
    }

    /// The alias of a control track: the short number that stands for its name in each
    /// object, the same on every khef. `None` for a media track, whose alias the conductor
    /// assigns in the plan (see [`crate::Plan::alias_of`]).
    ///
    /// - A report track: taken from the participant id, which is already a hash of the
    ///   node's key. A report must flow before its node is in any plan, so it cannot wait
    ///   for the conductor; and reports are rare enough that nine bytes do not matter.
    /// - The plan track: a fixed number.
    ///
    /// Both are above every alias a plan may assign.
    #[must_use]
    pub fn control_alias(&self) -> Option<u64> {
        match self {
            Self::Plan => Some(PLAN_ALIAS),
            Self::Report { participant } => {
                let mut prefix = [0u8; 8];
                for (slot, byte) in prefix.iter_mut().zip(participant.as_bytes()) {
                    *slot = *byte;
                }
                Some(REPORT_ALIAS | u64::from_be_bytes(prefix) >> 4)
            }
            Self::Audio { .. } | Self::Video { .. } => None,
        }
    }
}

/// A track of one meeting, convertible to and from its `MoQ` name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackName {
    pub meeting: MeetingTag,
    pub track: Track,
}

fn namespace(fields: &[&[u8]]) -> Option<Namespace> {
    Namespace::new(fields.iter().map(|f| f.to_vec()).collect()).ok()
}

impl TrackName {
    /// The `MoQ` name. `None` only if a rendition is empty or too long.
    #[must_use]
    pub fn to_full_name(&self) -> Option<FullTrackName> {
        let m = self.meeting.as_bytes();
        let (ns, name) = match &self.track {
            Track::Audio {
                source,
                participant,
                rendition,
            } => (
                namespace(&[m, AUDIO, source.as_bytes(), participant.as_bytes()])?,
                field(rendition)?,
            ),
            Track::Video {
                source,
                participant,
                rendition,
            } => (
                namespace(&[m, VIDEO, source.as_bytes(), participant.as_bytes()])?,
                field(rendition)?,
            ),
            Track::Report { participant } => (
                namespace(&[m, CTL, REPORT, participant.as_bytes()])?,
                CONTROL_NAME.to_vec(),
            ),
            Track::Plan => (namespace(&[m, CTL, PLAN])?, CONTROL_NAME.to_vec()),
        };
        FullTrackName::new(ns, name).ok()
    }

    /// Read a `MoQ` name. `None` if it does not have one of the shapes above.
    pub fn parse(name: &FullTrackName) -> Option<Self> {
        let fields: Vec<&[u8]> = name
            .namespace()
            .fields()
            .iter()
            .map(Vec::as_slice)
            .collect();
        let (meeting, rest) = fields.split_first()?;
        let meeting = MeetingTag::new(meeting)?;
        let track = match rest {
            [kind, source, participant] if *kind == AUDIO || *kind == VIDEO => {
                let source = Source::parse(source)?;
                let participant = ParticipantId::new(participant)?;
                let rendition = field(name.name())?;
                if *kind == AUDIO {
                    Track::Audio {
                        source,
                        participant,
                        rendition,
                    }
                } else {
                    Track::Video {
                        source,
                        participant,
                        rendition,
                    }
                }
            }
            [kind, what, participant]
                if *kind == CTL && *what == REPORT && name.name() == CONTROL_NAME =>
            {
                Track::Report {
                    participant: ParticipantId::new(participant)?,
                }
            }
            [kind, what] if *kind == CTL && *what == PLAN && name.name() == CONTROL_NAME => {
                Track::Plan
            }
            _ => return None,
        };
        Some(Self { meeting, track })
    }
}

impl MeetingTag {
    /// Prefix of every participant's audio tracks from `source`: what a client subscribes to
    /// for top-N audio.
    #[must_use]
    pub fn audio_prefix(&self, source: Source) -> Option<Namespace> {
        namespace(&[self.as_bytes(), AUDIO, source.as_bytes()])
    }

    /// Prefix of one participant's video tracks from `source`: the unit of a default route.
    #[must_use]
    pub fn video_prefix(&self, source: Source, participant: &ParticipantId) -> Option<Namespace> {
        namespace(&[
            self.as_bytes(),
            VIDEO,
            source.as_bytes(),
            participant.as_bytes(),
        ])
    }

    /// Prefix of every control track: what floods to all neighbours.
    #[must_use]
    pub fn control_prefix(&self) -> Option<Namespace> {
        namespace(&[self.as_bytes(), CTL])
    }

    /// Prefix of every track of the meeting.
    #[must_use]
    pub fn meeting_prefix(&self) -> Option<Namespace> {
        namespace(&[self.as_bytes()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meeting() -> MeetingTag {
        MeetingTag::new(b"m-7f3a").unwrap()
    }

    fn p(id: &str) -> ParticipantId {
        ParticipantId::new(id.as_bytes()).unwrap()
    }

    #[test]
    fn a_video_tracks_shape_is_carried_whole_or_not_at_all() {
        let shape = Shape {
            width: 1280,
            height: 720,
            per_second: 30,
            bitrate: 1_500_000,
        };
        let camera = |shape| MediaTrack {
            audio: false,
            source: Source::Main,
            rendition: b"large".to_vec(),
            shape,
        };
        for track in [camera(Some(shape)), camera(None)] {
            let read = MediaTrack::decode(&track.encode(Some(9))).unwrap();
            assert_eq!(read, (track, Some(9)));
        }
        // The same track, whatever is said of its shape.
        assert!(camera(Some(shape)).same_name(&camera(None)));
        assert!(!camera(None).same_name(&MediaTrack {
            rendition: b"small".to_vec(),
            ..camera(None)
        }));
        // A width with nothing else is not a shape.
        let mut w = Writer::default();
        w.bytes(MEDIA_RENDITION, b"large");
        w.int(MEDIA_KIND, 1);
        w.int(MEDIA_SOURCE, 0);
        w.int(MEDIA_WIDTH, 1280);
        assert_eq!(
            MediaTrack::decode(&w.finish()),
            Err(DecodeError::Invalid("track shape"))
        );
    }

    fn rendered(name: &FullTrackName) -> String {
        let fields: Vec<String> = name
            .namespace()
            .fields()
            .iter()
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect();
        format!(
            "({})/{}",
            fields.join(", "),
            String::from_utf8_lossy(name.name())
        )
    }

    fn all() -> Vec<(Track, &'static str)> {
        vec![
            (
                Track::Audio {
                    source: Source::Main,
                    participant: p("p12"),
                    rendition: b"opus".to_vec(),
                },
                "(m-7f3a, audio, main, p12)/opus",
            ),
            (
                Track::Video {
                    source: Source::Main,
                    participant: p("p12"),
                    rendition: b"360p".to_vec(),
                },
                "(m-7f3a, video, main, p12)/360p",
            ),
            (
                Track::Video {
                    source: Source::Screen,
                    participant: p("p12"),
                    rendition: b"1080p".to_vec(),
                },
                "(m-7f3a, video, screen, p12)/1080p",
            ),
            (
                Track::Report {
                    participant: p("p12"),
                },
                "(m-7f3a, ctl, report, p12)/0",
            ),
            (Track::Plan, "(m-7f3a, ctl, plan)/0"),
        ]
    }

    #[test]
    fn every_track_has_the_name_the_design_gives_and_parses_back() {
        for (track, expected) in all() {
            let name = TrackName {
                meeting: meeting(),
                track,
            };
            let full = name.to_full_name().unwrap();
            assert_eq!(rendered(&full), expected);
            assert_eq!(TrackName::parse(&full), Some(name));
        }
    }

    #[test]
    fn prefixes_match_exactly_the_tracks_they_should() {
        let m = meeting();
        let names: Vec<FullTrackName> = all()
            .into_iter()
            .map(|(track, _)| {
                TrackName {
                    meeting: m.clone(),
                    track,
                }
                .to_full_name()
                .unwrap()
            })
            .collect();
        let matched = |prefix: Namespace| -> Vec<bool> {
            names
                .iter()
                .map(|n| prefix.is_prefix_of(n.namespace()))
                .collect()
        };
        assert_eq!(
            matched(m.audio_prefix(Source::Main).unwrap()),
            [true, false, false, false, false]
        );
        assert_eq!(
            matched(m.video_prefix(Source::Main, &p("p12")).unwrap()),
            [false, true, false, false, false]
        );
        assert_eq!(
            matched(m.video_prefix(Source::Main, &p("p13")).unwrap()),
            [false; 5]
        );
        assert_eq!(
            matched(m.control_prefix().unwrap()),
            [false, false, false, true, true]
        );
        assert_eq!(matched(m.meeting_prefix().unwrap()), [true; 5]);
        assert_eq!(
            matched(MeetingTag::new(b"other").unwrap().meeting_prefix().unwrap()),
            [false; 5]
        );
    }

    #[test]
    fn names_of_other_shapes_do_not_parse() {
        let full = |fields: &[&str], name: &str| {
            let ns =
                Namespace::new(fields.iter().map(|f| f.as_bytes().to_vec()).collect()).unwrap();
            FullTrackName::new(ns, name.as_bytes().to_vec()).unwrap()
        };
        for (fields, name) in [
            (&["m", "audio", "main"][..], "opus"), // no participant
            (&["m", "audio", "main", "p", "x"][..], "opus"), // extra field
            (&["m", "audio", "desk", "p"][..], "opus"), // unknown source
            (&["m", "data", "main", "p"][..], "x"), // unknown kind
            (&["m", "audio", "main", "p"][..], ""), // empty rendition
            (&["m", "ctl", "plan"][..], "1"),      // wrong control name
            (&["m", "ctl", "report"][..], "0"),    // report without a participant
            (&["m", "ctl", "plan", "p"][..], "0"), // plan with a participant
            (&["m"][..], "0"),
        ] {
            assert_eq!(
                TrackName::parse(&full(fields, name)),
                None,
                "{fields:?}/{name}"
            );
        }
    }

    #[test]
    fn fields_must_be_between_one_and_64_bytes() {
        assert!(MeetingTag::new(b"").is_none());
        assert!(MeetingTag::new(&[7; 64]).is_some());
        assert!(MeetingTag::new(&[7; 65]).is_none());
        assert!(ParticipantId::new(b"").is_none());
        assert!(ParticipantId::new(&[7; 65]).is_none());
        let long = Track::Audio {
            source: Source::Main,
            participant: p("p"),
            rendition: vec![b'x'; 65],
        };
        assert!(
            TrackName {
                meeting: meeting(),
                track: long
            }
            .to_full_name()
            .is_none()
        );
    }

    #[test]
    fn the_meeting_tag_is_redacted_in_debug_output() {
        let name = TrackName {
            meeting: meeting(),
            track: Track::Plan,
        };
        let out = format!("{name:?}");
        assert!(!out.contains("7f3a") && !out.contains("109"), "{out}");
        assert!(out.contains("Plan"), "{out}");
    }

    fn media(audio: bool, source: Source, rendition: &str) -> MediaTrack {
        MediaTrack {
            audio,
            source,
            rendition: rendition.as_bytes().to_vec(),
            shape: None,
        }
    }

    #[test]
    fn a_media_track_round_trips_with_and_without_an_alias() {
        for track in [
            media(true, Source::Main, "opus"),
            media(false, Source::Screen, "1080p"),
            media(false, Source::Main, "a rendition nobody has heard of"),
        ] {
            assert_eq!(
                MediaTrack::decode(&track.encode(None)),
                Ok((track.clone(), None))
            );
            assert_eq!(
                MediaTrack::decode(&track.encode(Some(77))),
                Ok((track.clone(), Some(77)))
            );
        }
        // A microphone track is 10 bytes in a report.
        assert_eq!(media(true, Source::Main, "opus").encode(None).len(), 10);
    }

    #[test]
    fn a_media_track_with_a_bad_field_is_refused() {
        let good = media(true, Source::Main, "opus");
        assert!(MediaTrack::decode(&good.encode(None)).is_ok());
        let empty = MediaTrack {
            rendition: vec![],
            ..good
        };
        assert_eq!(
            MediaTrack::decode(&empty.encode(None)),
            Err(DecodeError::Invalid("track rendition"))
        );
        assert_eq!(
            MediaTrack::decode(&[]),
            Err(DecodeError::Missing("track kind"))
        );
    }

    #[test]
    fn a_media_track_and_a_track_convert_both_ways() {
        let participant = ParticipantId::new(&[9; 16]).unwrap();
        let mic = media(true, Source::Main, "opus");
        let track = mic.of(participant.clone());
        assert_eq!(
            track,
            Track::Audio {
                source: Source::Main,
                participant: participant.clone(),
                rendition: b"opus".to_vec()
            }
        );
        assert_eq!(track.media(), Some(mic));
        assert_eq!(track.participant(), Some(&participant));
        assert_eq!(Track::Plan.media(), None);
        assert_eq!(Track::Plan.participant(), None);
    }

    #[test]
    fn control_aliases_need_no_plan_and_are_above_every_alias_a_plan_assigns() {
        assert_eq!(Track::Plan.control_alias(), Some(PLAN_ALIAS));
        let report = |bytes: &[u8]| Track::Report {
            participant: ParticipantId::new(bytes).unwrap(),
        };
        let a = report(&[0x11; 16]).control_alias().unwrap();
        let b = report(&[0x12; 16]).control_alias().unwrap();
        assert!(a != b && a > PLAN_ALIAS && b > PLAN_ALIAS);
        // Ids that differ only after the first seven and a half bytes share an alias; real
        // ids are hashes, so that takes 2^60 tries to contrive.
        let mut near = [0x11; 16];
        near[15] = 0xff;
        assert_eq!(report(&near).control_alias(), Some(a));
        // A short id still has one.
        assert!(report(&[7]).control_alias().is_some());
        // Media tracks have none: theirs come from the plan.
        let mic = media(true, Source::Main, "opus").of(ParticipantId::new(&[9; 16]).unwrap());
        assert_eq!(mic.control_alias(), None);
    }
}
