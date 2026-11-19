//! Message parameters (section 9.20): optional values on request messages.
//!
//! Unlike key-value pairs, a parameter's value has no generic framing: each type defines its
//! own encoding, so an unknown type cannot be skipped and closes the session. The block is a
//! count followed by parameters in ascending type order, each type written as a delta.
//!
//! This profile implements the parameters below. The draft's other parameters (authorization
//! tokens, rendezvous and fill, range filters, namespace prefix updates) are refused.

use crate::{Error, Location, Reader, Result, put_vi64};
use core::time::Duration;

/// Whole milliseconds in `span`, as the wire carries them. A part of a millisecond is dropped.
pub(crate) fn millis(span: Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
}

const OBJECT_DELIVERY_TIMEOUT: u64 = 0x02;
const SUBGROUP_DELIVERY_TIMEOUT: u64 = 0x06;
const EXPIRES: u64 = 0x08;
const LARGEST_OBJECT: u64 = 0x09;
const FORWARD: u64 = 0x10;
const SUBSCRIBER_PRIORITY: u64 = 0x20;
const LOCATION_FILTER: u64 = 0x21;
const GROUP_ORDER: u64 = 0x22;
const NEW_GROUP_REQUEST: u64 = 0x32;
const INCLUDE_PROPERTIES: u64 = 0x35;

/// Defined by the draft but outside this profile.
const UNSUPPORTED: &[u64] = &[0x03, 0x04, 0x0a, 0x23, 0x25, 0x26, 0x27, 0x28, 0x29, 0x34];

/// Parameters that may appear in SUBSCRIBE.
pub const SUBSCRIBE_PARAMS: &[u64] = &[
    OBJECT_DELIVERY_TIMEOUT,
    SUBGROUP_DELIVERY_TIMEOUT,
    FORWARD,
    SUBSCRIBER_PRIORITY,
    LOCATION_FILTER,
    GROUP_ORDER,
    NEW_GROUP_REQUEST,
    INCLUDE_PROPERTIES,
];
/// Parameters that may appear in SUBSCRIBE_OK and REQUEST_OK.
pub const OK_PARAMS: &[u64] = &[EXPIRES, LARGEST_OBJECT];
/// Parameters that may appear in PUBLISH.
pub const PUBLISH_PARAMS: &[u64] = &[
    OBJECT_DELIVERY_TIMEOUT,
    SUBGROUP_DELIVERY_TIMEOUT,
    EXPIRES,
    LARGEST_OBJECT,
    FORWARD,
    SUBSCRIBER_PRIORITY,
    LOCATION_FILTER,
    GROUP_ORDER,
];
/// Parameters that may appear in SUBSCRIBE_TRACKS.
pub const SUBSCRIBE_TRACKS_PARAMS: &[u64] = &[FORWARD, GROUP_ORDER, INCLUDE_PROPERTIES];
/// Parameters that may appear in REQUEST_UPDATE. Which of them apply depends on the request
/// being updated, which the codec does not know; the khef checks that.
pub const REQUEST_UPDATE_PARAMS: &[u64] = &[
    OBJECT_DELIVERY_TIMEOUT,
    SUBGROUP_DELIVERY_TIMEOUT,
    FORWARD,
    SUBSCRIBER_PRIORITY,
    LOCATION_FILTER,
    NEW_GROUP_REQUEST,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupOrder {
    Ascending,
    Descending,
}

/// Which objects a subscription covers (section 9.20.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationFilter {
    /// No filter.
    None,
    /// Start this many groups before the next group; 0 is the next group, 1 the current one.
    RelativeStart { groups_back: u64 },
    /// From `start`, open-ended.
    AbsoluteStart(Location),
    /// From `start` to the end of group `start.group + end_group_delta`.
    AbsoluteStartGroupEnd {
        start: Location,
        end_group_delta: u64,
    },
    /// From `start` to object `end_object` of group `start.group + end_group_delta`.
    AbsoluteRange {
        start: Location,
        end_group_delta: u64,
        end_object: u64,
    },
    /// From the next object, open-ended.
    NextObject,
}

/// The parameters of one message. `None` means the parameter is absent and its default, which
/// depends on the message, applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params {
    /// How long an object may wait before it is dropped. Sent in whole milliseconds.
    pub object_delivery_timeout: Option<Duration>,
    /// How long a subgroup may wait before it is dropped. Sent in whole milliseconds.
    pub subgroup_delivery_timeout: Option<Duration>,
    /// How long until the sender ends the subscription; zero means unknown or never. Sent in
    /// whole milliseconds.
    pub expires: Option<Duration>,
    pub largest_object: Option<Location>,
    /// Whether objects are forwarded (false pauses the subscription).
    pub forward: Option<bool>,
    /// Lower is more important.
    pub subscriber_priority: Option<u8>,
    pub location_filter: Option<LocationFilter>,
    pub group_order: Option<GroupOrder>,
    /// Largest group id the subscriber knows, plus one; 0 if none. Asks for a new group.
    pub new_group_request: Option<u64>,
    pub include_properties: Option<bool>,
}

fn flag(byte: u8) -> Result<bool> {
    match byte {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Error::Violation("parameter value must be 0 or 1")),
    }
}

impl LocationFilter {
    fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Self::None => put_vi64(out, 0),
            Self::RelativeStart { groups_back } => {
                put_vi64(out, 1);
                put_vi64(out, groups_back);
            }
            Self::AbsoluteStart(start) => {
                put_vi64(out, 2);
                start.encode(out);
            }
            Self::AbsoluteStartGroupEnd {
                start,
                end_group_delta,
            } => {
                put_vi64(out, 3);
                start.encode(out);
                put_vi64(out, end_group_delta);
            }
            Self::AbsoluteRange {
                start,
                end_group_delta,
                end_object,
            } => {
                put_vi64(out, 4);
                start.encode(out);
                put_vi64(out, end_group_delta);
                put_vi64(out, end_object);
            }
            Self::NextObject => put_vi64(out, 5),
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let end = |start: Location, delta: u64| {
            start
                .group
                .checked_add(delta)
                .map(|_| delta)
                .ok_or(Error::Violation("location filter end group overflows"))
        };
        Ok(match r.vi64()? {
            0 => Self::None,
            1 => Self::RelativeStart {
                groups_back: r.vi64()?,
            },
            2 => Self::AbsoluteStart(Location::decode(r)?),
            3 => {
                let start = Location::decode(r)?;
                Self::AbsoluteStartGroupEnd {
                    start,
                    end_group_delta: end(start, r.vi64()?)?,
                }
            }
            4 => {
                let start = Location::decode(r)?;
                let end_group_delta = end(start, r.vi64()?)?;
                Self::AbsoluteRange {
                    start,
                    end_group_delta,
                    end_object: r.vi64()?,
                }
            }
            5 => Self::NextObject,
            _ => return Err(Error::Violation("unknown location filter type")),
        })
    }
}

impl Params {
    /// The set parameters as (type, encoded value), in ascending type order.
    fn encoded(&self) -> Vec<(u64, Vec<u8>)> {
        let vi = |v: u64| {
            let mut out = vec![];
            put_vi64(&mut out, v);
            out
        };
        let mut list = vec![];
        if let Some(v) = self.object_delivery_timeout {
            list.push((OBJECT_DELIVERY_TIMEOUT, vi(millis(v))));
        }
        if let Some(v) = self.subgroup_delivery_timeout {
            list.push((SUBGROUP_DELIVERY_TIMEOUT, vi(millis(v))));
        }
        if let Some(v) = self.expires {
            list.push((EXPIRES, vi(millis(v))));
        }
        if let Some(v) = self.largest_object {
            let mut out = vec![];
            v.encode(&mut out);
            list.push((LARGEST_OBJECT, out));
        }
        if let Some(v) = self.forward {
            list.push((FORWARD, vec![u8::from(v)]));
        }
        if let Some(v) = self.subscriber_priority {
            list.push((SUBSCRIBER_PRIORITY, vec![v]));
        }
        if let Some(v) = self.location_filter {
            let mut out = vec![];
            v.encode(&mut out);
            list.push((LOCATION_FILTER, out));
        }
        if let Some(v) = self.group_order {
            list.push((
                GROUP_ORDER,
                vec![if v == GroupOrder::Ascending { 1 } else { 2 }],
            ));
        }
        if let Some(v) = self.new_group_request {
            list.push((NEW_GROUP_REQUEST, vi(v)));
        }
        if let Some(v) = self.include_properties {
            list.push((INCLUDE_PROPERTIES, vec![u8::from(v)]));
        }
        list
    }

    /// Append the count and the parameters. Fails if a parameter is set that `allowed` (the
    /// message's list) does not include.
    pub fn encode(&self, allowed: &[u64], out: &mut Vec<u8>) -> Result<()> {
        let list = self.encoded();
        if list.iter().any(|(ty, _)| !allowed.contains(ty)) {
            return Err(Error::Violation("parameter not allowed in this message"));
        }
        put_vi64(out, list.len() as u64);
        let mut prev = 0;
        for (ty, value) in &list {
            put_vi64(out, ty - prev);
            out.extend_from_slice(value);
            prev = *ty;
        }
        Ok(())
    }

    /// Read the count and the parameters of a message whose list of permitted types is
    /// `allowed`.
    pub fn decode(r: &mut Reader<'_>, allowed: &[u64]) -> Result<Self> {
        let mut p = Self::default();
        let count = r.vi64()?;
        let mut ty = 0u64;
        for i in 0..count {
            let delta = r.vi64()?;
            ty = ty
                .checked_add(delta)
                .ok_or(Error::Violation("parameter type exceeds 2^64 - 1"))?;
            if i > 0 && delta == 0 {
                return Err(Error::Violation("parameter repeated"));
            }
            if UNSUPPORTED.contains(&ty) {
                return Err(Error::Violation("parameter not supported by this profile"));
            }
            if !SUBSCRIBE_PARAMS.contains(&ty) && !OK_PARAMS.contains(&ty) {
                return Err(Error::Violation("unknown message parameter"));
            }
            if !allowed.contains(&ty) {
                return Err(Error::Violation("parameter not allowed in this message"));
            }
            match ty {
                OBJECT_DELIVERY_TIMEOUT => {
                    p.object_delivery_timeout = Some(Duration::from_millis(r.vi64()?))
                }
                SUBGROUP_DELIVERY_TIMEOUT => {
                    p.subgroup_delivery_timeout = Some(Duration::from_millis(r.vi64()?))
                }
                EXPIRES => p.expires = Some(Duration::from_millis(r.vi64()?)),
                LARGEST_OBJECT => p.largest_object = Some(Location::decode(r)?),
                FORWARD => p.forward = Some(flag(r.u8()?)?),
                SUBSCRIBER_PRIORITY => p.subscriber_priority = Some(r.u8()?),
                LOCATION_FILTER => p.location_filter = Some(LocationFilter::decode(r)?),
                GROUP_ORDER => {
                    p.group_order = Some(match r.u8()? {
                        1 => GroupOrder::Ascending,
                        2 => GroupOrder::Descending,
                        _ => return Err(Error::Violation("group order must be 1 or 2")),
                    });
                }
                NEW_GROUP_REQUEST => p.new_group_request = Some(r.vi64()?),
                INCLUDE_PROPERTIES => p.include_properties = Some(flag(r.u8()?)?),
                _ => return Err(Error::Violation("unknown message parameter")),
            }
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[u64] = &[0x02, 0x06, 0x08, 0x09, 0x10, 0x20, 0x21, 0x22, 0x32, 0x35];

    fn everything() -> Params {
        Params {
            object_delivery_timeout: Some(Duration::from_millis(500)),
            subgroup_delivery_timeout: Some(Duration::from_millis(2000)),
            expires: Some(Duration::from_millis(60_000)),
            largest_object: Some(Location {
                group: 9,
                object: 3,
            }),
            forward: Some(false),
            subscriber_priority: Some(200),
            location_filter: Some(LocationFilter::AbsoluteRange {
                start: Location {
                    group: 5,
                    object: 1,
                },
                end_group_delta: 2,
                end_object: 7,
            }),
            group_order: Some(GroupOrder::Descending),
            new_group_request: Some(10),
            include_properties: Some(true),
        }
    }

    fn body(parts: &[u8]) -> Result<Params> {
        let mut outer = Reader::new(parts);
        Params::decode(&mut outer.sub(parts.len())?, ALL)
    }

    #[test]
    fn every_parameter_round_trips() {
        let mut out = vec![];
        everything().encode(ALL, &mut out).unwrap();
        assert_eq!(body(&out), Ok(everything()));
    }

    #[test]
    fn no_parameters_is_a_single_zero() {
        let mut out = vec![];
        Params::default().encode(&[], &mut out).unwrap();
        assert_eq!(out, [0]);
        assert_eq!(body(&out), Ok(Params::default()));
    }

    #[test]
    fn types_are_deltas_and_single_bytes_are_not_varints() {
        let p = Params {
            forward: Some(true),
            subscriber_priority: Some(200),
            ..Params::default()
        };
        let mut out = vec![];
        p.encode(ALL, &mut out).unwrap();
        // count 2 | 0x10: 1 | +0x10: 200 as one raw byte
        assert_eq!(out, [2, 0x10, 1, 0x10, 200]);
    }

    #[test]
    fn every_location_filter_form_round_trips() {
        let start = Location {
            group: 4,
            object: 2,
        };
        for f in [
            LocationFilter::None,
            LocationFilter::RelativeStart { groups_back: 1 },
            LocationFilter::AbsoluteStart(start),
            LocationFilter::AbsoluteStartGroupEnd {
                start,
                end_group_delta: 3,
            },
            LocationFilter::AbsoluteRange {
                start,
                end_group_delta: 0,
                end_object: 9,
            },
            LocationFilter::NextObject,
        ] {
            let p = Params {
                location_filter: Some(f),
                ..Params::default()
            };
            let mut out = vec![];
            p.encode(ALL, &mut out).unwrap();
            assert_eq!(body(&out), Ok(p), "{f:?}");
        }
    }

    #[test]
    fn a_parameter_outside_the_message_list_is_refused_both_ways() {
        let p = Params {
            expires: Some(Duration::from_millis(1)),
            ..Params::default()
        };
        assert!(matches!(
            p.encode(SUBSCRIBE_PARAMS, &mut vec![]),
            Err(Error::Violation(_))
        ));
        let mut out = vec![];
        p.encode(OK_PARAMS, &mut out).unwrap();
        let mut outer = Reader::new(&out);
        let got = Params::decode(&mut outer.sub(out.len()).unwrap(), SUBSCRIBE_PARAMS);
        assert_eq!(
            got,
            Err(Error::Violation("parameter not allowed in this message"))
        );
    }

    #[test]
    fn bad_blocks_are_violations() {
        // unknown type 0x40; unsupported type 0x03; repeated 0x20; forward = 2; group order = 0;
        // location filter type 6; end group overflow; truncated value
        let cases: &[(&[u8], &str)] = &[
            (&[1, 0x40, 0], "unknown message parameter"),
            (&[1, 0x03, 0], "parameter not supported by this profile"),
            (&[2, 0x20, 1, 0x00, 1], "parameter repeated"),
            (&[1, 0x10, 2], "parameter value must be 0 or 1"),
            (&[1, 0x22, 0], "group order must be 1 or 2"),
            (&[1, 0x21, 6], "unknown location filter type"),
        ];
        for (bytes, why) in cases {
            assert_eq!(body(bytes), Err(Error::Violation(why)), "{bytes:x?}");
        }
        let mut overflow = vec![1, 0x21, 3];
        put_vi64(&mut overflow, u64::MAX);
        overflow.extend_from_slice(&[0, 1]);
        assert_eq!(
            body(&overflow),
            Err(Error::Violation("location filter end group overflows"))
        );
        assert!(matches!(body(&[1, 0x02]), Err(Error::Violation(_))));
        assert!(matches!(body(&[2, 0x02, 5]), Err(Error::Violation(_))));
    }
}
