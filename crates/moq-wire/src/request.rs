//! Request messages: SUBSCRIBE and its replies (sections 9.3, 9.4, 9.6, 9.7).

use crate::frame::{Frame, encode_frame};
use crate::params::millis;
use crate::params::{OK_PARAMS, Params, SUBSCRIBE_PARAMS};
use crate::publish::{
    GOAWAY, GoAway, PUBLISH, PUBLISH_DONE, Publish, PublishDone, REQUEST_UPDATE, RequestUpdate,
    SUBSCRIBE_TRACKS, SubscribeTracks,
};
use crate::{Error, FullTrackName, Reader, Result, Setup, decode_kvps, put_vi64};
use core::time::Duration;

pub const REQUEST_ERROR: u64 = 0x05;
pub const REQUEST_OK: u64 = 0x07;
pub const SUBSCRIBE: u64 = 0x03;
pub const SUBSCRIBE_OK: u64 = 0x04;

/// Error code that carries a [`Redirect`].
pub const ERROR_REDIRECT: u64 = 0x34;
/// Longest reason phrase, in bytes.
pub const MAX_REASON_BYTES: usize = 1024;

/// Asks for a track by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscribe {
    pub request_id: u64,
    pub track: FullTrackName,
    pub params: Params,
}

/// Accepts a subscription and names the alias its objects will carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeOk {
    pub track_alias: u64,
    pub params: Params,
    /// Track properties as encoded key-value pairs, forwarded untouched.
    pub track_properties: Vec<u8>,
}

/// Accepts a request other than SUBSCRIBE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOk {
    pub params: Params,
    /// Track properties as encoded key-value pairs; empty except when answering `TRACK_STATUS`.
    pub track_properties: Vec<u8>,
}

/// Where to retry a request (section 9.4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// Empty means the current session's URI.
    pub connect_uri: Vec<u8>,
    pub target: FullTrackName,
}

/// Refuses a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub code: u64,
    /// The shortest time to wait before sending the request again; `None` means do not retry.
    /// On the wire this is whole milliseconds plus one, with 0 for "do not retry".
    pub retry_after: Option<Duration>,
    pub reason: String,
    /// Present exactly when `code` is [`ERROR_REDIRECT`].
    pub redirect: Option<Redirect>,
}

/// Track properties run to the end of the message; check they parse, keep them encoded.
pub fn track_properties(body: &mut Reader<'_>) -> Result<Vec<u8>> {
    decode_kvps(&mut body.clone())?;
    Ok(body.rest().to_vec())
}

/// A reason phrase (section 8.5): a length and up to 1024 bytes of text.
pub fn encode_reason(reason: &str, out: &mut Vec<u8>) -> Result<()> {
    if reason.len() > MAX_REASON_BYTES {
        return Err(Error::Violation("reason phrase exceeds 1024 bytes"));
    }
    put_vi64(out, reason.len() as u64);
    out.extend_from_slice(reason.as_bytes());
    Ok(())
}

pub fn decode_reason(body: &mut Reader<'_>) -> Result<String> {
    let len = body.vi64()?;
    if len > MAX_REASON_BYTES as u64 {
        return Err(Error::Violation("reason phrase exceeds 1024 bytes"));
    }
    Ok(
        String::from_utf8_lossy(body.take(usize::try_from(len).unwrap_or(usize::MAX))?)
            .into_owned(),
    )
}

impl Subscribe {
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        put_vi64(out, self.request_id);
        self.track.encode(out);
        self.params.encode(SUBSCRIBE_PARAMS, out)
    }

    fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            request_id: body.vi64()?,
            track: FullTrackName::decode(body)?,
            params: Params::decode(body, SUBSCRIBE_PARAMS)?,
        })
    }
}

impl SubscribeOk {
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        put_vi64(out, self.track_alias);
        self.params.encode(OK_PARAMS, out)?;
        out.extend_from_slice(&self.track_properties);
        Ok(())
    }

    fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            track_alias: body.vi64()?,
            params: Params::decode(body, OK_PARAMS)?,
            track_properties: track_properties(body)?,
        })
    }
}

impl RequestOk {
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        self.params.encode(OK_PARAMS, out)?;
        out.extend_from_slice(&self.track_properties);
        Ok(())
    }

    fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            params: Params::decode(body, OK_PARAMS)?,
            track_properties: track_properties(body)?,
        })
    }
}

impl RequestError {
    fn encode_body(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.redirect.is_some() != (self.code == ERROR_REDIRECT) {
            return Err(Error::Violation(
                "a redirect goes with the REDIRECT error code and no other",
            ));
        }
        put_vi64(out, self.code);
        put_vi64(
            out,
            self.retry_after
                .map_or(0, |wait| millis(wait).saturating_add(1)),
        );
        encode_reason(&self.reason, out)?;
        if let Some(redirect) = &self.redirect {
            put_vi64(out, redirect.connect_uri.len() as u64);
            out.extend_from_slice(&redirect.connect_uri);
            redirect.target.encode(out);
        }
        Ok(())
    }

    fn decode_body(body: &mut Reader<'_>) -> Result<Self> {
        let code = body.vi64()?;
        let retry_after = body.vi64()?.checked_sub(1).map(Duration::from_millis);
        let reason = decode_reason(body)?;
        let redirect = if code == ERROR_REDIRECT {
            let len = body.vi64()?;
            let connect_uri = body
                .take(usize::try_from(len).unwrap_or(usize::MAX))?
                .to_vec();
            Some(Redirect {
                connect_uri,
                target: FullTrackName::decode(body)?,
            })
        } else {
            None
        };
        Ok(Self {
            code,
            retry_after,
            reason,
            redirect,
        })
    }
}

/// Any control message of this profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Setup(Setup),
    Subscribe(Subscribe),
    SubscribeOk(SubscribeOk),
    RequestOk(RequestOk),
    RequestError(RequestError),
    RequestUpdate(RequestUpdate),
    Publish(Publish),
    PublishDone(PublishDone),
    SubscribeTracks(SubscribeTracks),
    GoAway(GoAway),
}

impl Message {
    /// Append the whole message, framing included.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Violation`] if a field is over its length limit, a parameter is set
    /// that the message does not allow, or the body is longer than 65535 bytes.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut body = vec![];
        let ty = match self {
            Self::Setup(m) => return m.encode(out),
            Self::Subscribe(m) => m.encode_body(&mut body).map(|()| SUBSCRIBE)?,
            Self::SubscribeOk(m) => m.encode_body(&mut body).map(|()| SUBSCRIBE_OK)?,
            Self::RequestOk(m) => m.encode_body(&mut body).map(|()| REQUEST_OK)?,
            Self::RequestError(m) => m.encode_body(&mut body).map(|()| REQUEST_ERROR)?,
            Self::RequestUpdate(m) => m.encode_body(&mut body).map(|()| REQUEST_UPDATE)?,
            Self::Publish(m) => m.encode_body(&mut body).map(|()| PUBLISH)?,
            Self::PublishDone(m) => m.encode_body(&mut body).map(|()| PUBLISH_DONE)?,
            Self::SubscribeTracks(m) => m.encode_body(&mut body).map(|()| SUBSCRIBE_TRACKS)?,
            Self::GoAway(m) => m.encode_body(&mut body).map(|()| GOAWAY)?,
        };
        encode_frame(ty, &body, out)
    }

    /// Decode a framed message.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Violation`] if the body is malformed, ends early, or has bytes left
    /// over. An unknown message type is also a violation: the draft allows no control message
    /// to be ignored.
    pub fn decode(mut frame: Frame<'_>) -> Result<Self> {
        let message = match frame.ty {
            crate::SETUP => return Setup::decode(frame).map(Self::Setup),
            SUBSCRIBE => Self::Subscribe(Subscribe::decode_body(&mut frame.body)?),
            SUBSCRIBE_OK => Self::SubscribeOk(SubscribeOk::decode_body(&mut frame.body)?),
            REQUEST_OK => Self::RequestOk(RequestOk::decode_body(&mut frame.body)?),
            REQUEST_ERROR => Self::RequestError(RequestError::decode_body(&mut frame.body)?),
            REQUEST_UPDATE => Self::RequestUpdate(RequestUpdate::decode_body(&mut frame.body)?),
            PUBLISH => Self::Publish(Publish::decode_body(&mut frame.body)?),
            PUBLISH_DONE => Self::PublishDone(PublishDone::decode_body(&mut frame.body)?),
            SUBSCRIBE_TRACKS => {
                Self::SubscribeTracks(SubscribeTracks::decode_body(&mut frame.body)?)
            }
            GOAWAY => Self::GoAway(GoAway::decode_body(&mut frame.body)?),
            _ => return Err(Error::Violation("unknown or unsupported message type")),
        };
        frame.finish()?;
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{GroupOrder, LocationFilter};
    use crate::{Kvp, Location, Namespace, Value, encode_kvps};

    fn track() -> FullTrackName {
        let ns = Namespace::new(
            ["m-7f3a", "video", "main", "p12"]
                .iter()
                .map(|f| f.as_bytes().to_vec())
                .collect(),
        );
        FullTrackName::new(ns.unwrap(), b"360p".to_vec()).unwrap()
    }

    fn properties() -> Vec<u8> {
        let mut out = vec![];
        encode_kvps(
            &[Kvp {
                ty: 0x3802,
                value: Value::Int(7),
            }],
            &mut out,
        )
        .unwrap();
        out
    }

    fn samples() -> Vec<Message> {
        vec![
            Message::Subscribe(Subscribe {
                request_id: 4,
                track: track(),
                params: Params {
                    subscriber_priority: Some(64),
                    group_order: Some(GroupOrder::Ascending),
                    location_filter: Some(LocationFilter::RelativeStart { groups_back: 1 }),
                    new_group_request: Some(0),
                    ..Params::default()
                },
            }),
            Message::SubscribeOk(SubscribeOk {
                track_alias: 41,
                params: Params {
                    largest_object: Some(Location {
                        group: 9,
                        object: 2,
                    }),
                    ..Params::default()
                },
                track_properties: properties(),
            }),
            Message::RequestOk(RequestOk {
                params: Params {
                    expires: Some(Duration::from_secs(30)),
                    ..Params::default()
                },
                track_properties: vec![],
            }),
            Message::RequestError(RequestError {
                code: 0x10,
                retry_after: None,
                reason: String::from("no such track"),
                redirect: None,
            }),
            Message::RequestError(RequestError {
                code: ERROR_REDIRECT,
                retry_after: Some(Duration::ZERO),
                reason: String::new(),
                redirect: Some(Redirect {
                    connect_uri: vec![],
                    target: track(),
                }),
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
    fn a_body_shorter_or_longer_than_its_length_is_a_violation() {
        for m in samples() {
            let bytes = wire(&m);
            let mut r = Reader::new(&bytes);
            let ty = r.vi64().unwrap();
            let body = &bytes[bytes.len() - r.remaining() + 2..];
            // one byte dropped from the end, and one stray byte added
            for altered in [body[..body.len() - 1].to_vec(), [body, &[0x7f]].concat()] {
                let mut reframed = vec![];
                encode_frame(ty, &altered, &mut reframed).unwrap();
                assert!(
                    matches!(
                        parse(&reframed),
                        Err(Error::Violation(_) | Error::KeyValueFormatting(_))
                    ),
                    "{m:?}"
                );
            }
        }
    }

    #[test]
    fn subscribe_refuses_a_reply_parameter() {
        let m = Message::Subscribe(Subscribe {
            request_id: 0,
            track: track(),
            params: Params {
                expires: Some(Duration::from_millis(1)),
                ..Params::default()
            },
        });
        assert!(matches!(m.encode(&mut vec![]), Err(Error::Violation(_))));
    }

    #[test]
    fn request_error_checks_the_reason_length_and_the_redirect_pairing() {
        let long = RequestError {
            code: 1,
            retry_after: None,
            reason: "x".repeat(1025),
            redirect: None,
        };
        assert!(matches!(
            Message::RequestError(long).encode(&mut vec![]),
            Err(Error::Violation(_))
        ));
        let unpaired = RequestError {
            code: ERROR_REDIRECT,
            retry_after: None,
            reason: String::new(),
            redirect: None,
        };
        assert!(matches!(
            Message::RequestError(unpaired).encode(&mut vec![]),
            Err(Error::Violation(_))
        ));
        // on the wire: code 1, retry 0, reason length 1025
        let mut body = vec![1, 0];
        put_vi64(&mut body, 1025);
        body.extend_from_slice(&[b'x'; 1025]);
        let mut bytes = vec![];
        encode_frame(REQUEST_ERROR, &body, &mut bytes).unwrap();
        assert_eq!(
            parse(&bytes),
            Err(Error::Violation("reason phrase exceeds 1024 bytes"))
        );
    }

    #[test]
    fn malformed_track_properties_are_a_violation() {
        // alias 1, no parameters, then a property of odd type 1 claiming 5 bytes with none present
        let mut bytes = vec![];
        encode_frame(SUBSCRIBE_OK, &[1, 0, 1, 5], &mut bytes).unwrap();
        assert!(matches!(parse(&bytes), Err(Error::Violation(_))));
    }

    #[test]
    fn the_retry_wait_is_sent_as_milliseconds_plus_one_with_zero_for_never() {
        let error = |retry_after| RequestError {
            code: 1,
            retry_after,
            reason: String::new(),
            redirect: None,
        };
        // code, retry interval, reason length
        let cases = [
            (None, vec![1, 0, 0]),
            (Some(Duration::ZERO), vec![1, 1, 0]),
            (Some(Duration::from_millis(99)), vec![1, 100, 0]),
            (Some(Duration::from_micros(99_900)), vec![1, 100, 0]), // part of a millisecond is dropped
        ];
        for (retry_after, body) in cases {
            let mut out = vec![];
            Message::RequestError(error(retry_after))
                .encode(&mut out)
                .unwrap();
            assert_eq!(&out[3..], body, "{retry_after:?}");
        }
        let mut bytes = vec![];
        encode_frame(REQUEST_ERROR, &[1, 100, 0], &mut bytes).unwrap();
        assert_eq!(
            parse(&bytes),
            Ok(Message::RequestError(error(Some(Duration::from_millis(
                99
            )))))
        );
    }

    #[test]
    fn an_unknown_message_type_is_a_violation() {
        let mut bytes = vec![];
        encode_frame(0x7e, &[], &mut bytes).unwrap();
        assert!(matches!(parse(&bytes), Err(Error::Violation(_))));
    }
}
