//! Locations, track namespaces and full track names (sections 8.2 and 8.7).

use crate::{Error, Reader, Result, put_vi64};

/// Most fields a track namespace may have.
pub const MAX_NAMESPACE_FIELDS: usize = 32;
/// Most bytes a namespace, or a namespace plus track name, may total.
pub const MAX_NAME_BYTES: usize = 4096;

/// An object's position in a track. Ordered by group, then object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    pub group: u64,
    pub object: u64,
}

impl Location {
    pub fn encode(&self, out: &mut Vec<u8>) {
        put_vi64(out, self.group);
        put_vi64(out, self.object);
    }

    /// Read a group id and an object id.
    ///
    /// # Errors
    ///
    /// Fails if `r` runs out.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            group: r.vi64()?,
            object: r.vi64()?,
        })
    }
}

/// A track namespace: up to 32 non-empty byte fields, at most 4096 bytes in total.
///
/// Also used for namespace prefixes, which may have no fields.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Namespace(Vec<Vec<u8>>);

impl Namespace {
    /// A namespace made of `fields`.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Violation`] if there are more than 32 fields, a field is empty, or
    /// the fields total more than 4096 bytes.
    pub fn new(fields: Vec<Vec<u8>>) -> Result<Self> {
        if fields.len() > MAX_NAMESPACE_FIELDS {
            return Err(Error::Violation("track namespace has more than 32 fields"));
        }
        if fields.iter().any(Vec::is_empty) {
            return Err(Error::Violation("track namespace field is empty"));
        }
        if fields.iter().map(Vec::len).sum::<usize>() > MAX_NAME_BYTES {
            return Err(Error::Violation("track namespace exceeds 4096 bytes"));
        }
        Ok(Self(fields))
    }

    #[must_use]
    pub fn fields(&self) -> &[Vec<u8>] {
        &self.0
    }

    /// Total bytes across the fields.
    pub fn byte_len(&self) -> usize {
        self.0.iter().map(Vec::len).sum()
    }

    /// Whether every field of `self` equals the corresponding leading field of `other`.
    #[must_use]
    pub fn is_prefix_of(&self, other: &Self) -> bool {
        other
            .0
            .get(..self.0.len())
            .is_some_and(|head| head == self.0)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        put_vi64(out, self.0.len() as u64);
        for field in &self.0 {
            put_vi64(out, field.len() as u64);
            out.extend_from_slice(field);
        }
    }

    /// Read a field count and that many length-prefixed fields.
    ///
    /// # Errors
    ///
    /// Fails if `r` runs out, or with [`Error::Violation`] on the conditions of
    /// [`Namespace::new`].
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let count = r.vi64()?;
        if count > MAX_NAMESPACE_FIELDS as u64 {
            return Err(Error::Violation("track namespace has more than 32 fields"));
        }
        let mut fields = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
        let mut total = 0usize;
        for _ in 0..count {
            let len = r.vi64()?;
            if len == 0 {
                return Err(Error::Violation("track namespace field is empty"));
            }
            total = total.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
            if total > MAX_NAME_BYTES {
                return Err(Error::Violation("track namespace exceeds 4096 bytes"));
            }
            fields.push(r.take(usize::try_from(len).unwrap_or(usize::MAX))?.to_vec());
        }
        Ok(Self(fields))
    }
}

/// A namespace plus a track name; together at most 4096 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FullTrackName {
    namespace: Namespace,
    name: Vec<u8>,
}

impl FullTrackName {
    /// A track `name` within `namespace`.
    ///
    /// # Errors
    ///
    /// Fails with [`Error::Violation`] if the namespace and name total more than 4096 bytes.
    pub fn new(namespace: Namespace, name: Vec<u8>) -> Result<Self> {
        if namespace.byte_len() + name.len() > MAX_NAME_BYTES {
            return Err(Error::Violation("full track name exceeds 4096 bytes"));
        }
        Ok(Self { namespace, name })
    }

    #[must_use]
    pub const fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        self.namespace.encode(out);
        put_vi64(out, self.name.len() as u64);
        out.extend_from_slice(&self.name);
    }

    /// Read a namespace, then a length-prefixed track name.
    ///
    /// # Errors
    ///
    /// Fails if `r` runs out, the namespace is invalid (see [`Namespace::decode`]), or the
    /// namespace and name total more than 4096 bytes.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let namespace = Namespace::decode(r)?;
        let len = r.vi64()?;
        if len > (MAX_NAME_BYTES - namespace.byte_len()) as u64 {
            return Err(Error::Violation("full track name exceeds 4096 bytes"));
        }
        let name = r.take(usize::try_from(len).unwrap_or(usize::MAX))?.to_vec();
        Ok(Self { namespace, name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(fields: &[&str]) -> Namespace {
        Namespace::new(fields.iter().map(|f| f.as_bytes().to_vec()).collect()).unwrap()
    }

    #[test]
    fn location_orders_by_group_then_object_and_round_trips() {
        let (a, b, c) = (
            Location {
                group: 1,
                object: 9,
            },
            Location {
                group: 2,
                object: 0,
            },
            Location {
                group: 2,
                object: 1,
            },
        );
        assert!(a < b && b < c);
        let mut out = vec![];
        c.encode(&mut out);
        assert_eq!(Location::decode(&mut Reader::new(&out)), Ok(c));
    }

    #[test]
    fn namespace_round_trips_and_has_the_expected_bytes() {
        let n = ns(&["m-7f3a", "audio"]);
        let mut out = vec![];
        n.encode(&mut out);
        assert_eq!(out, b"\x02\x06m-7f3a\x05audio");
        assert_eq!(Namespace::decode(&mut Reader::new(&out)), Ok(n));
    }

    #[test]
    fn prefix_matching_is_by_whole_fields() {
        assert!(ns(&["m", "audio"]).is_prefix_of(&ns(&["m", "audio", "main"])));
        assert!(ns(&[]).is_prefix_of(&ns(&["m"])));
        assert!(!ns(&["m", "aud"]).is_prefix_of(&ns(&["m", "audio"])));
        assert!(!ns(&["m", "audio", "main"]).is_prefix_of(&ns(&["m", "audio"])));
    }

    #[test]
    fn the_limits_are_enforced_when_building_and_when_decoding() {
        assert!(Namespace::new(vec![b"x".to_vec(); 32]).is_ok());
        assert!(matches!(
            Namespace::new(vec![b"x".to_vec(); 33]),
            Err(Error::Violation(_))
        ));
        assert!(matches!(
            Namespace::new(vec![vec![]]),
            Err(Error::Violation(_))
        ));
        assert!(matches!(
            Namespace::new(vec![vec![0; 4097]]),
            Err(Error::Violation(_))
        ));
        // 33 fields; an empty field; a field longer than the limit
        assert!(matches!(
            Namespace::decode(&mut Reader::new(&[33])),
            Err(Error::Violation(_))
        ));
        assert!(matches!(
            Namespace::decode(&mut Reader::new(&[1, 0])),
            Err(Error::Violation(_))
        ));
        assert!(matches!(
            Namespace::decode(&mut Reader::new(&[1, 0x90, 0x01])),
            Err(Error::Violation(_))
        ));
    }

    #[test]
    fn a_truncated_namespace_asks_for_more() {
        let mut out = vec![];
        ns(&["meeting", "video"]).encode(&mut out);
        for cut in 0..out.len() {
            assert_eq!(
                Namespace::decode(&mut Reader::new(&out[..cut])),
                Err(Error::NeedMore),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn full_track_name_round_trips_and_counts_both_parts_against_the_limit() {
        let f = FullTrackName::new(ns(&["m", "video", "main", "p12"]), b"360p".to_vec()).unwrap();
        let mut out = vec![];
        f.encode(&mut out);
        assert_eq!(FullTrackName::decode(&mut Reader::new(&out)), Ok(f));
        let big = Namespace::new(vec![vec![1; 4000]]).unwrap();
        assert!(FullTrackName::new(big.clone(), vec![0; 96]).is_ok());
        assert!(matches!(
            FullTrackName::new(big.clone(), vec![0; 97]),
            Err(Error::Violation(_))
        ));
        let mut wire = vec![];
        big.encode(&mut wire);
        put_vi64(&mut wire, 97);
        assert!(matches!(
            FullTrackName::decode(&mut Reader::new(&wire)),
            Err(Error::Violation(_))
        ));
    }
}
