//! Signed envelopes for control-plane objects.
//!
//! The object on a control track is `payload length | payload | signature`. The signature is
//! over the bytes as sent, so no canonical encoding is needed: a receiver verifies what it
//! received and only then decodes it. The signed message also names what kind of object it
//! is, the meeting, and the sequence number, so an object cannot be replayed as another kind,
//! into another meeting, or as a newer one.
//!
//! This crate does no cryptography. Keys, signing and hashing are supplied through the
//! [`Signer`] and [`Verifier`] traits by whoever owns the node's identity; the simulator
//! supplies fakes.

use crate::fields::DecodeError;
use crate::{MeetingTag, ParticipantId};
use palaver_moq_wire::{Reader, put_vi64};

/// Length of a signature in bytes (Ed25519).
pub const SIGNATURE_BYTES: usize = 64;

/// A node's per-meeting public key (Ed25519).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    pub(crate) fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature(pub [u8; SIGNATURE_BYTES]);

/// Signs with this node's per-meeting key.
pub trait Signer {
    fn public_key(&self) -> PublicKey;
    fn sign(&self, message: &[u8]) -> Signature;
}

/// Checks signatures and derives ids from keys.
pub trait Verifier {
    fn verify(&self, key: &PublicKey, message: &[u8], signature: &Signature) -> bool;
    /// The participant id that belongs to `key` (the first 16 bytes of a hash of it).
    fn participant_id(&self, key: &PublicKey) -> ParticipantId;
}

/// What a signed object is. Part of the signed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Report,
    Plan,
}

impl Kind {
    fn label(self) -> &'static [u8] {
        match self {
            Self::Report => b"palaver-report-v1",
            Self::Plan => b"palaver-plan-v1",
        }
    }
}

/// The bytes that are signed: label, a zero byte, the meeting tag with its length, the
/// sequence number, then the payload.
fn message(kind: Kind, meeting: &MeetingTag, sequence: u64, payload: &[u8]) -> Vec<u8> {
    let tag = meeting.as_bytes();
    let mut out = Vec::with_capacity(kind.label().len() + tag.len() + payload.len() + 10);
    out.extend_from_slice(kind.label());
    out.push(0);
    out.push(u8::try_from(tag.len()).unwrap_or(u8::MAX)); // a tag is at most 64 bytes
    out.extend_from_slice(tag);
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Wrap `payload` in a signed envelope.
pub(crate) fn seal(
    kind: Kind,
    meeting: &MeetingTag,
    sequence: u64,
    payload: &[u8],
    signer: &dyn Signer,
) -> Vec<u8> {
    let signature = signer.sign(&message(kind, meeting, sequence, payload));
    let mut out = Vec::with_capacity(payload.len() + SIGNATURE_BYTES + 4);
    put_vi64(&mut out, payload.len() as u64);
    out.extend_from_slice(payload);
    out.extend_from_slice(&signature.0);
    out
}

/// An envelope split into its parts, signature not yet checked.
pub(crate) struct Sealed<'a> {
    pub(crate) payload: &'a [u8],
    signature: Signature,
}

impl<'a> Sealed<'a> {
    pub(crate) fn split(envelope: &'a [u8]) -> Result<Self, DecodeError> {
        let mut outer = Reader::new(envelope);
        let mut r = outer
            .sub(envelope.len())
            .map_err(|_| DecodeError::Malformed)?;
        let len = r.vi64().map_err(|_| DecodeError::Malformed)?;
        let payload = r
            .take(usize::try_from(len).unwrap_or(usize::MAX))
            .map_err(|_| DecodeError::Malformed)?;
        let signature = r
            .rest()
            .try_into()
            .map(Signature)
            .map_err(|_| DecodeError::Malformed)?;
        Ok(Self { payload, signature })
    }

    /// Check the signature against `key` for the given kind, meeting and sequence number.
    pub(crate) fn verify(
        &self,
        kind: Kind,
        meeting: &MeetingTag,
        sequence: u64,
        key: &PublicKey,
        verifier: &dyn Verifier,
    ) -> Result<(), DecodeError> {
        if verifier.verify(
            key,
            &message(kind, meeting, sequence, self.payload),
            &self.signature,
        ) {
            Ok(())
        } else {
            Err(DecodeError::BadSignature("signature does not verify"))
        }
    }
}

/// A stand-in for real signatures, for tests and the simulator. NOT cryptography: the "public
/// key" is all it takes to sign.
pub mod fake {
    use super::*;

    fn mix(key: &PublicKey, message: &[u8]) -> [u8; SIGNATURE_BYTES] {
        let mut state = 0xcbf2_9ce4_8422_2325u64;
        let mut out = [0u8; SIGNATURE_BYTES];
        for (i, slot) in out.iter_mut().enumerate() {
            for byte in key.0.iter().chain(message).chain(&[i as u8]) {
                state = (state ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
            }
            *slot = (state >> 24) as u8;
        }
        out
    }

    /// Derives everything from one seed byte.
    #[derive(Debug, Clone, Copy)]
    pub struct FakeIdentity(pub u8);

    impl Signer for FakeIdentity {
        fn public_key(&self) -> PublicKey {
            PublicKey([self.0; 32])
        }

        fn sign(&self, message: &[u8]) -> Signature {
            Signature(mix(&self.public_key(), message))
        }
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub struct FakeVerifier;

    impl Verifier for FakeVerifier {
        fn verify(&self, key: &PublicKey, message: &[u8], signature: &Signature) -> bool {
            mix(key, message) == signature.0
        }

        fn participant_id(&self, key: &PublicKey) -> ParticipantId {
            let mut id = [0x5au8; 16];
            for (slot, byte) in id.iter_mut().zip(key.0) {
                *slot ^= byte;
            }
            ParticipantId::from_key_hash(id)
        }
    }
}
