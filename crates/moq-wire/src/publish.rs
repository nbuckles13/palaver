//! PUBLISH, PUBLISH_DONE, SUBSCRIBE_TRACKS, REQUEST_UPDATE and GOAWAY (sections 9.2, 9.5, 9.8,
//! 9.9, 9.18).

use crate::params::millis;
use crate::params::{PUBLISH_PARAMS, Params, REQUEST_UPDATE_PARAMS, SUBSCRIBE_TRACKS_PARAMS};
use crate::request::{decode_reason, encode_reason, track_properties};
use crate::{Error, FullTrackName, Namespace, Reader, Result, put_vi64};
use core::time::Duration;

pub const REQUEST_UPDATE: u64 = 0x02;
pub const PUBLISH_DONE: u64 = 0x0b;
pub const GOAWAY: u64 = 0x10;
pub const PUBLISH: u64 = 0x1d;
pub const SUBSCRIBE_TRACKS: u64 = 0x51;

/// Longest New Session URI in a GOAWAY, in bytes.
pub const MAX_URI_BYTES: usize = 8192;

/// Changes parameters of a request already made on the same stream. A parameter left out
/// keeps its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestUpdate {
    pub request_id: u64,
    pub params: Params,
}

/// A publisher starts a subscription for a track it has, without being asked for it by name.
/// This is how tracks under a SUBSCRIBE_TRACKS prefix arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    pub request_id: u64,
    pub track: FullTrackName,
    /// The alias the track's objects will carry.
    pub track_alias: u64,
    pub params: Params,
    /// Track properties as encoded key-value pairs, forwarded untouched.
    pub track_properties: Vec<u8>,
}

/// The publisher's last message on a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDone {
    pub status: u64,
    /// Data streams the publisher opened for the subscription; `u64::MAX` if it cannot say.
    pub stream_count: u64,
    pub reason: String,
}

/// Asks for a PUBLISH for every track, present and future, under a namespace prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeTracks {
    pub request_id: u64,
    pub prefix: Namespace,
    pub params: Params,
}

/// The sender intends to close the session, or one request, soon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoAway {
    /// Where to continue; empty means the current URI.
    pub new_session_uri: Vec<u8>,
    /// How long the sender will wait; zero means no specific time, migrate promptly. Sent in
    /// whole milliseconds.
    pub timeout: Duration,
}

impl RequestUpdate {
    pub(crate) fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        put_vi64(out, self.request_id);
        self.params.encode(REQUEST_UPDATE_PARAMS, out)
    }

    pub(crate) fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            request_id: body.vi64()?,
            params: Params::decode(body, REQUEST_UPDATE_PARAMS)?,
        })
    }
}

impl Publish {
    pub(crate) fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        put_vi64(out, self.request_id);
        self.track.encode(out);
        put_vi64(out, self.track_alias);
        self.params.encode(PUBLISH_PARAMS, out)?;
        out.extend_from_slice(&self.track_properties);
        Ok(())
    }

    pub(crate) fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            request_id: body.vi64()?,
            track: FullTrackName::decode(body)?,
            track_alias: body.vi64()?,
            params: Params::decode(body, PUBLISH_PARAMS)?,
            track_properties: track_properties(body)?,
        })
    }
}

impl PublishDone {
    pub(crate) fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        put_vi64(out, self.status);
        put_vi64(out, self.stream_count);
        encode_reason(&self.reason, out)
    }

    pub(crate) fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            status: body.vi64()?,
            stream_count: body.vi64()?,
            reason: decode_reason(body)?,
        })
    }
}

impl SubscribeTracks {
    pub(crate) fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        put_vi64(out, self.request_id);
        self.prefix.encode(out);
        self.params.encode(SUBSCRIBE_TRACKS_PARAMS, out)
    }

    pub(crate) fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            request_id: body.vi64()?,
            prefix: Namespace::decode(body)?,
            params: Params::decode(body, SUBSCRIBE_TRACKS_PARAMS)?,
        })
    }
}

impl GoAway {
    pub(crate) fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.new_session_uri.len() > MAX_URI_BYTES {
            return Err(Error::Violation("new session URI exceeds 8192 bytes"));
        }
        put_vi64(out, self.new_session_uri.len() as u64);
        out.extend_from_slice(&self.new_session_uri);
        put_vi64(out, millis(self.timeout));
        Ok(())
    }

    pub(crate) fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        let len = body.vi64()?;
        if len > MAX_URI_BYTES as u64 {
            return Err(Error::Violation("new session URI exceeds 8192 bytes"));
        }
        Ok(Self {
            new_session_uri: body.take(len as usize)?.to_vec(),
            timeout: Duration::from_millis(body.vi64()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, encode_frame};
    use crate::params::GroupOrder;
    use crate::{Kvp, Location, Message, Value, encode_kvps};

    fn namespace(fields: &[&str]) -> Namespace {
        Namespace::new(fields.iter().map(|f| f.as_bytes().to_vec()).collect()).unwrap()
    }

    fn samples() -> Vec<Message> {
        let mut properties = vec![];
        encode_kvps(
            &[Kvp {
                ty: 0x3802,
                value: Value::Int(7),
            }],
            &mut properties,
        )
        .unwrap();
        vec![
            Message::RequestUpdate(RequestUpdate {
                request_id: 6,
                params: Params {
                    forward: Some(false),
                    new_group_request: Some(12),
                    ..Params::default()
                },
            }),
            Message::Publish(Publish {
                request_id: 8,
                track: FullTrackName::new(
                    namespace(&["m-7f3a", "audio", "main", "p12"]),
                    b"opus".to_vec(),
                )
                .unwrap(),
                track_alias: 17,
                params: Params {
                    largest_object: Some(Location {
                        group: 300,
                        object: 0,
                    }),
                    group_order: Some(GroupOrder::Ascending),
                    ..Params::default()
                },
                track_properties: properties,
            }),
            Message::PublishDone(PublishDone {
                status: 2,
                stream_count: u64::MAX,
                reason: String::from("track ended"),
            }),
            Message::SubscribeTracks(SubscribeTracks {
                request_id: 10,
                prefix: namespace(&["m-7f3a", "audio", "main"]),
                params: Params {
                    forward: Some(true),
                    include_properties: Some(false),
                    ..Params::default()
                },
            }),
            Message::GoAway(GoAway {
                new_session_uri: vec![],
                timeout: Duration::from_millis(3000),
            }),
            Message::GoAway(GoAway {
                new_session_uri: b"moqt://khef.example/m".to_vec(),
                timeout: Duration::from_millis(0),
            }),
        ]
    }

    fn wire(m: &Message) -> Vec<u8> {
        let mut out = vec![];
        m.encode(&mut out).unwrap();
        out
    }

    fn parse(bytes: &[u8]) -> Result<Message> {
        Message::decode(Frame::decode(&mut Reader::new(bytes))?)
    }

    #[test]
    fn every_message_round_trips() {
        for m in samples() {
            assert_eq!(parse(&wire(&m)), Ok(m));
        }
    }

    #[test]
    fn a_message_cut_anywhere_asks_for_more() {
        for m in samples() {
            let bytes = wire(&m);
            for cut in 0..bytes.len() {
                assert_eq!(
                    parse(&bytes[..cut]),
                    Err(Error::NeedMore),
                    "{m:?} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn a_stray_byte_after_the_body_is_a_violation() {
        for m in samples() {
            let bytes = wire(&m);
            let mut r = Reader::new(&bytes);
            let ty = r.vi64().unwrap();
            r.take(2).unwrap();
            let mut body = r.rest().to_vec();
            body.push(0x7f);
            let mut reframed = vec![];
            encode_frame(ty, &body, &mut reframed).unwrap();
            assert!(
                matches!(
                    parse(&reframed),
                    Err(Error::Violation(_) | Error::KeyValueFormatting(_))
                ),
                "{m:?}"
            );
        }
    }

    #[test]
    fn each_message_refuses_a_parameter_it_does_not_allow() {
        let prefix = namespace(&["m"]);
        let with_expires = Params {
            expires: Some(Duration::from_millis(1)),
            ..Params::default()
        };
        let with_priority = Params {
            subscriber_priority: Some(1),
            ..Params::default()
        };
        let with_new_group = Params {
            new_group_request: Some(1),
            ..Params::default()
        };
        let bad = [
            Message::RequestUpdate(RequestUpdate {
                request_id: 0,
                params: with_expires.clone(),
            }),
            Message::SubscribeTracks(SubscribeTracks {
                request_id: 0,
                prefix: prefix.clone(),
                params: with_priority,
            }),
            Message::SubscribeTracks(SubscribeTracks {
                request_id: 0,
                prefix: prefix.clone(),
                params: with_expires,
            }),
            Message::Publish(Publish {
                request_id: 0,
                track: FullTrackName::new(prefix, b"t".to_vec()).unwrap(),
                track_alias: 0,
                params: with_new_group,
                track_properties: vec![],
            }),
        ];
        for m in bad {
            assert!(
                matches!(m.encode(&mut vec![]), Err(Error::Violation(_))),
                "{m:?}"
            );
        }
    }

    #[test]
    fn subscribe_tracks_accepts_an_empty_prefix() {
        let m = Message::SubscribeTracks(SubscribeTracks {
            request_id: 0,
            prefix: namespace(&[]),
            params: Params::default(),
        });
        assert_eq!(parse(&wire(&m)), Ok(m));
    }

    #[test]
    fn goaway_limits_the_uri_length_both_ways() {
        let long = GoAway {
            new_session_uri: vec![b'a'; MAX_URI_BYTES + 1],
            timeout: Duration::from_millis(0),
        };
        assert!(matches!(
            Message::GoAway(long).encode(&mut vec![]),
            Err(Error::Violation(_))
        ));
        let mut body = vec![];
        put_vi64(&mut body, MAX_URI_BYTES as u64 + 1);
        body.extend_from_slice(&vec![b'a'; MAX_URI_BYTES + 1]);
        body.push(0);
        let mut bytes = vec![];
        encode_frame(GOAWAY, &body, &mut bytes).unwrap();
        assert_eq!(
            parse(&bytes),
            Err(Error::Violation("new session URI exceeds 8192 bytes"))
        );
    }
}
