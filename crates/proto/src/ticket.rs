//! The invite ticket: what a person pastes to join a meeting (design section 4).
//!
//! It holds the meeting secret, the iroh-relay to use, and one or more entry points: members
//! to connect to first, each a public key with the addresses it was last seen at. It is a
//! snapshot made when someone copies an invite; it keeps working while any entry point can
//! be reached.
//!
//! As text it is `palaver:` followed by base32 (letters and the digits 2 to 7, either case).
//! Inside: a version byte, the fields in the same numbered encoding as reports and plans, and
//! a checksum, so that a ticket cut short by a chat program is refused rather than half used.
//!
//! **The text contains the meeting secret.** Anyone who has it can join. It is handed out as
//! a [`Secret`] so that it is not logged by accident.

use crate::fields::{Writer, fields};
use crate::report::{MAX_ADDRESSES, MAX_RELAY_URL_BYTES, decode_address, encode_address};
use crate::sign::PublicKey;
use crate::{Redacted, Secret};
use palaver_moq_wire::Value;
use std::net::SocketAddr;

/// Length of the meeting secret.
pub const SECRET_BYTES: usize = 32;
/// Most entry points in a ticket. A reader keeps the first this many and ignores the rest.
pub const MAX_ENTRY_POINTS: usize = 4;
/// Longest ticket text accepted, in characters.
pub const MAX_TEXT_CHARS: usize = 4096;

const PREFIX: &str = "palaver:";
const VERSION: u8 = 1;
const CHECKSUM_BYTES: usize = 4;

// Field numbers. Even numbers carry integers, odd ones bytes. Never reuse a number.
const SECRET: u64 = 1;
const RELAY_URL: u64 = 3;
const ENTRY_POINT: u64 = 5;

const ENTRY_KEY: u64 = 1;
const ENTRY_ADDRESS: u64 = 3;

/// A member to connect to first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPoint {
    pub key: PublicKey,
    /// Where it was last seen; hints only. May be empty if it can be reached through the
    /// iroh-relay by its key.
    pub addresses: Vec<Redacted<SocketAddr>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub secret: Secret<[u8; SECRET_BYTES]>,
    /// The iroh-relay through which entry points can be reached by key, if one is used.
    pub relay_url: Option<String>,
    pub entry_points: Vec<EntryPoint>,
}

/// Why a piece of text is not a usable ticket. The wording is for showing to the person who
/// pasted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// It does not start with `palaver:`.
    NotATicket,
    /// Characters are missing or changed: most likely it was not copied whole.
    Damaged,
    /// Made by a newer version of the app than this one understands.
    UnsupportedVersion(u8),
    /// Well formed, but something in it is missing or not allowed.
    Invalid(&'static str),
}

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(8).div_ceil(5));
    let (mut bits, mut count) = (0u32, 0u32);
    let mut push = |index: u32| {
        if let Some(letter) = ALPHABET.get(index as usize & 31) {
            out.push(char::from(*letter));
        }
    };
    for byte in bytes {
        bits = bits << 8 | u32::from(*byte);
        count += 8;
        while count >= 5 {
            count -= 5;
            push(bits >> count);
        }
    }
    if count > 0 {
        push(bits << (5 - count));
    }
    out
}

/// `None` on a character outside the alphabet, or on leftover bits that are not zero (which
/// no encoder produces, and would let two texts mean the same bytes).
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len().saturating_mul(5) / 8);
    let (mut bits, mut count) = (0u32, 0u32);
    for letter in text.bytes() {
        let value = match letter.to_ascii_lowercase() {
            lower @ b'a'..=b'z' => lower - b'a',
            digit @ b'2'..=b'7' => digit - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5 | u32::from(value)) & 0xffff;
        count += 5;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    let leftover = bits & ((1 << count) - 1);
    (count < 5 && leftover == 0).then_some(out)
}

/// CRC-32 (the common IEEE one). It catches accidents, not tampering: the secret inside the
/// ticket is what stands against that.
fn checksum(bytes: &[u8]) -> [u8; CHECKSUM_BYTES] {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    (!crc).to_be_bytes()
}

impl EntryPoint {
    fn encode(&self) -> Vec<u8> {
        let addresses: Vec<Vec<u8>> = self
            .addresses
            .iter()
            .take(MAX_ADDRESSES)
            .map(|address| encode_address(address.reveal()))
            .collect();
        let mut w = Writer::default();
        w.bytes(ENTRY_KEY, &self.key.0);
        for address in &addresses {
            w.bytes(ENTRY_ADDRESS, address);
        }
        w.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, TicketError> {
        let (mut key, mut addresses) = (None, Vec::new());
        for pair in fields(bytes) {
            let pair = pair.map_err(|_| TicketError::Damaged)?;
            match (pair.ty, pair.value) {
                (ENTRY_KEY, Value::Bytes(b)) => key = PublicKey::from_slice(b),
                (ENTRY_ADDRESS, Value::Bytes(b)) => {
                    let address =
                        decode_address(b).ok_or(TicketError::Invalid("entry point address"))?;
                    if addresses.len() < MAX_ADDRESSES {
                        addresses.push(Redacted::new(address));
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            key: key.ok_or(TicketError::Invalid("entry point has no key"))?,
            addresses,
        })
    }
}

impl Ticket {
    /// The ticket as text to share. It contains the meeting secret.
    pub fn to_text(&self) -> Secret<String> {
        let entries: Vec<Vec<u8>> = self
            .entry_points
            .iter()
            .take(MAX_ENTRY_POINTS)
            .map(EntryPoint::encode)
            .collect();
        let mut w = Writer::default();
        w.bytes(SECRET, self.secret.expose());
        if let Some(url) = &self.relay_url {
            w.bytes(RELAY_URL, url.as_bytes());
        }
        for entry in &entries {
            w.bytes(ENTRY_POINT, entry);
        }
        let mut bytes = vec![VERSION];
        bytes.extend_from_slice(&w.finish());
        let check = checksum(&bytes);
        bytes.extend_from_slice(&check);
        Secret::new(format!("{PREFIX}{}", base32_encode(&bytes)))
    }

    /// Read a ticket from text a person pasted. Space around it and the case of its letters
    /// do not matter.
    pub fn parse(text: &str) -> Result<Self, TicketError> {
        let text = text.trim();
        if text.len() > MAX_TEXT_CHARS {
            return Err(TicketError::Invalid("too long"));
        }
        let body = text
            .get(..PREFIX.len())
            .filter(|start| start.eq_ignore_ascii_case(PREFIX))
            .and_then(|_| text.get(PREFIX.len()..))
            .ok_or(TicketError::NotATicket)?;
        let bytes = base32_decode(body).ok_or(TicketError::Damaged)?;
        let (covered, check) = bytes
            .len()
            .checked_sub(CHECKSUM_BYTES)
            .and_then(|at| bytes.split_at_checked(at))
            .ok_or(TicketError::Damaged)?;
        if check != checksum(covered) {
            return Err(TicketError::Damaged);
        }
        let (version, payload) = covered.split_first().ok_or(TicketError::Damaged)?;
        if *version != VERSION {
            return Err(TicketError::UnsupportedVersion(*version));
        }

        let (mut secret, mut relay_url, mut entry_points) = (None, None, Vec::new());
        for pair in fields(payload) {
            let pair = pair.map_err(|_| TicketError::Damaged)?;
            match (pair.ty, pair.value) {
                (SECRET, Value::Bytes(b)) => {
                    let bytes: [u8; SECRET_BYTES] = b
                        .try_into()
                        .map_err(|_| TicketError::Invalid("secret length"))?;
                    secret = Some(Secret::new(bytes));
                }
                (RELAY_URL, Value::Bytes(b)) => {
                    if b.len() > MAX_RELAY_URL_BYTES {
                        return Err(TicketError::Invalid("iroh-relay URL too long"));
                    }
                    let url = core::str::from_utf8(b)
                        .map_err(|_| TicketError::Invalid("iroh-relay URL is not UTF-8"))?;
                    relay_url = Some(url.to_owned());
                }
                (ENTRY_POINT, Value::Bytes(b)) => {
                    let entry = EntryPoint::decode(b)?;
                    if entry_points.len() < MAX_ENTRY_POINTS {
                        entry_points.push(entry);
                    }
                }
                _ => {}
            }
        }
        if entry_points.is_empty() {
            return Err(TicketError::Invalid("no entry point"));
        }
        Ok(Self {
            secret: secret.ok_or(TicketError::Invalid("no secret"))?,
            relay_url,
            entry_points,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> Redacted<SocketAddr> {
        Redacted::new(text.parse().unwrap())
    }

    fn entry(seed: u8, addresses: &[&str]) -> EntryPoint {
        EntryPoint {
            key: PublicKey([seed; 32]),
            addresses: addresses.iter().map(|a| address(a)).collect(),
        }
    }

    fn sample() -> Ticket {
        Ticket {
            secret: Secret::new([7; SECRET_BYTES]),
            relay_url: Some("https://relay.example.org".to_owned()),
            entry_points: vec![
                entry(1, &["192.168.1.5:4433", "[2001:db8::5]:4433"]),
                entry(2, &["10.0.0.9:50123"]),
            ],
        }
    }

    /// The bytes inside a ticket's text, checksum included.
    fn inside(ticket: &Ticket) -> Vec<u8> {
        base32_decode(&ticket.to_text().expose()[PREFIX.len()..]).unwrap()
    }

    /// Text for `bytes` (version and payload), with a correct checksum.
    fn text_for(mut bytes: Vec<u8>) -> String {
        let check = checksum(&bytes);
        bytes.extend_from_slice(&check);
        format!("{PREFIX}{}", base32_encode(&bytes))
    }

    #[test]
    fn a_ticket_round_trips_through_text() {
        let text = sample().to_text();
        assert!(text.expose().starts_with("palaver:"));
        assert_eq!(Ticket::parse(text.expose()), Ok(sample()));
        // Two entry points with three addresses and a relay: short enough to paste anywhere.
        assert!(text.expose().len() < 300, "{} chars", text.expose().len());
    }

    #[test]
    fn a_ticket_without_a_relay_or_addresses_round_trips() {
        let ticket = Ticket {
            relay_url: None,
            entry_points: vec![entry(1, &[])],
            ..sample()
        };
        assert_eq!(Ticket::parse(ticket.to_text().expose()), Ok(ticket));
    }

    #[test]
    fn case_and_surrounding_space_do_not_matter() {
        let text = sample().to_text();
        let shouted = format!("  \n{}\t ", text.expose().to_uppercase());
        assert_eq!(Ticket::parse(&shouted), Ok(sample()));
    }

    #[test]
    fn text_that_is_not_a_ticket_is_told_apart_from_a_damaged_one() {
        for text in ["", "hello", "https://example.org", "palaver", "palave:abc"] {
            assert_eq!(
                Ticket::parse(text),
                Err(TicketError::NotATicket),
                "{text:?}"
            );
        }
        for text in [
            "palaver:",
            "palaver:abc",
            "palaver:not base32!",
            "palaver:a1b0",
        ] {
            assert_eq!(Ticket::parse(text), Err(TicketError::Damaged), "{text:?}");
        }
    }

    #[test]
    fn a_ticket_cut_short_anywhere_is_refused() {
        let text = sample().to_text();
        let text = text.expose();
        for keep in PREFIX.len()..text.len() {
            assert_eq!(
                Ticket::parse(&text[..keep]),
                Err(TicketError::Damaged),
                "cut to {keep} characters"
            );
        }
    }

    #[test]
    fn a_ticket_with_any_one_character_changed_is_refused() {
        let text = sample().to_text();
        let text = text.expose();
        for at in PREFIX.len()..text.len() {
            let mut changed = text.clone().into_bytes();
            changed[at] = if changed[at] == b'a' { b'b' } else { b'a' };
            let changed = String::from_utf8(changed).unwrap();
            assert_eq!(
                Ticket::parse(&changed),
                Err(TicketError::Damaged),
                "character {at}"
            );
        }
    }

    #[test]
    fn a_ticket_from_a_newer_version_says_so() {
        let mut bytes = inside(&sample());
        bytes.truncate(bytes.len() - CHECKSUM_BYTES);
        bytes[0] = 2;
        assert_eq!(
            Ticket::parse(&text_for(bytes)),
            Err(TicketError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn fields_from_a_newer_version_are_skipped() {
        let mut bytes = inside(&sample());
        bytes.truncate(bytes.len() - CHECKSUM_BYTES);
        // Append unknown fields 200 (integer) and 201 (bytes): deltas 195 and 1 after field 5.
        bytes.extend_from_slice(&[0x80, 195, 7, 1, 2, 0xaa, 0xbb]);
        assert_eq!(Ticket::parse(&text_for(bytes)), Ok(sample()));
    }

    #[test]
    fn a_ticket_must_have_a_secret_and_an_entry_point() {
        let no_entry = Ticket {
            entry_points: vec![],
            ..sample()
        };
        assert_eq!(
            Ticket::parse(no_entry.to_text().expose()),
            Err(TicketError::Invalid("no entry point"))
        );
        // Only an entry point, no secret.
        let entry = entry(1, &["10.0.0.9:50123"]).encode();
        let mut w = Writer::default();
        w.bytes(ENTRY_POINT, &entry);
        let bytes = [vec![VERSION], w.finish()].concat();
        assert_eq!(
            Ticket::parse(&text_for(bytes)),
            Err(TicketError::Invalid("no secret"))
        );
    }

    #[test]
    fn a_secret_of_the_wrong_length_is_refused() {
        let entry = entry(1, &[]).encode();
        let mut w = Writer::default();
        w.bytes(SECRET, &[7; 16]);
        w.bytes(ENTRY_POINT, &entry);
        let bytes = [vec![VERSION], w.finish()].concat();
        assert_eq!(
            Ticket::parse(&text_for(bytes)),
            Err(TicketError::Invalid("secret length"))
        );
    }

    #[test]
    fn extra_entry_points_and_addresses_are_dropped_not_refused() {
        let many: Vec<String> = (0..12).map(|n| format!("10.0.0.{n}:4433")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let ticket = Ticket {
            entry_points: (1..=6).map(|seed| entry(seed, &many)).collect(),
            ..sample()
        };
        let read = Ticket::parse(ticket.to_text().expose()).unwrap();
        assert_eq!(read.entry_points.len(), MAX_ENTRY_POINTS);
        assert!(
            read.entry_points
                .iter()
                .all(|e| e.addresses.len() == MAX_ADDRESSES)
        );
        assert_eq!(read.entry_points[0].key, PublicKey([1; 32]));
        // The largest ticket still fits the limit on text.
        assert!(ticket.to_text().expose().len() < MAX_TEXT_CHARS);
    }

    #[test]
    fn text_longer_than_the_limit_is_refused_before_any_work() {
        let long = format!("{PREFIX}{}", "a".repeat(MAX_TEXT_CHARS));
        assert_eq!(Ticket::parse(&long), Err(TicketError::Invalid("too long")));
    }

    #[test]
    fn the_secret_and_addresses_do_not_appear_in_debug_output() {
        let out = format!("{:?} {:?}", sample(), sample().to_text());
        assert!(
            !out.contains("192.168") && !out.contains("10.0.0.9"),
            "{out}"
        );
        assert!(!out.contains("7, 7") && !out.contains("palaver:"), "{out}");
    }

    #[test]
    fn base32_round_trips_every_length_and_refuses_what_no_encoder_writes() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base32_decode(&base32_encode(&bytes)), Some(bytes));
        }
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
        // A lone character carries 5 bits, which no whole number of bytes leaves over.
        assert_eq!(base32_decode("a"), None);
        // Leftover bits must be zero.
        assert_eq!(base32_decode("mzxw6ytboj"), None);
        assert_eq!(base32_decode("mz=="), None);
        assert_eq!(base32_decode("m1"), None);
    }

    #[test]
    fn the_checksum_is_the_standard_crc32() {
        assert_eq!(checksum(b"123456789"), 0xcbf4_3926u32.to_be_bytes());
    }
}
