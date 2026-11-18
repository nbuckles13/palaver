//! Track naming (design section 4).
//!
//! A MoQ track is a namespace tuple plus a name. MoQ has no wildcards, only namespace-prefix
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
use palaver_moq_wire::{FullTrackName, Namespace};

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

/// The opaque value that identifies a meeting on the wire. It is derived from the meeting
/// secret (the derivation lives with admission, not here), so every member computes the same
/// tag and it means nothing to anyone outside the meeting.
/// Redacted in logs: it would let two log lines be tied to the same meeting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MeetingTag(Redacted<Vec<u8>>);

impl MeetingTag {
    /// `None` if `bytes` is empty or longer than [`MAX_FIELD_BYTES`].
    pub fn new(bytes: &[u8]) -> Option<Self> {
        field(bytes).map(|b| Self(Redacted::new(b)))
    }

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
    fn as_bytes(self) -> &'static [u8] {
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

/// A track of one meeting, convertible to and from its MoQ name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackName {
    pub meeting: MeetingTag,
    pub track: Track,
}

fn namespace(fields: &[&[u8]]) -> Option<Namespace> {
    Namespace::new(fields.iter().map(|f| f.to_vec()).collect()).ok()
}

impl TrackName {
    /// The MoQ name. `None` only if a rendition is empty or too long.
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

    /// Read a MoQ name. `None` if it does not have one of the shapes above.
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
    pub fn audio_prefix(&self, source: Source) -> Option<Namespace> {
        namespace(&[self.as_bytes(), AUDIO, source.as_bytes()])
    }

    /// Prefix of one participant's video tracks from `source`: the unit of a default route.
    pub fn video_prefix(&self, source: Source, participant: &ParticipantId) -> Option<Namespace> {
        namespace(&[
            self.as_bytes(),
            VIDEO,
            source.as_bytes(),
            participant.as_bytes(),
        ])
    }

    /// Prefix of every control track: what floods to all neighbours.
    pub fn control_prefix(&self) -> Option<Namespace> {
        namespace(&[self.as_bytes(), CTL])
    }

    /// Prefix of every track of the meeting.
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
}
