//! A node's key, and what is derived from keys and from the meeting secret.
//!
//! - A node has one Ed25519 key per meeting. It signs the node's reports (and plans, when
//!   the node is conductor), and it is the key the transport knows the node by.
//! - A **participant id** is the first 16 bytes of the SHA-256 of the public key, so an id
//!   can be checked against the key that signs for it.
//! - The **meeting tag** is an HKDF-SHA-256 output of the meeting secret. Every member
//!   computes the same tag; it tells an outsider nothing about the secret.

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use palaver_proto::{
    MeetingTag, ParticipantId, PublicKey, SECRET_BYTES, Secret, Signature, Signer, Verifier,
};
use sha2::{Digest, Sha256};

/// Length of the meeting tag.
pub const MEETING_TAG_BYTES: usize = 16;

/// Says what the HKDF output is for. Changing it changes every meeting tag.
const MEETING_TAG_LABEL: &[u8] = b"palaver meeting tag v1";

/// The operating system would not give random bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRandomness;

/// A node's per-meeting key pair.
#[derive(Clone)]
pub struct Identity {
    key: SigningKey,
}

impl core::fmt::Debug for Identity {
    /// Shows the public half only.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Identity")
            .field("public", &self.public_key())
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A new key from the operating system's random source.
    ///
    /// # Errors
    ///
    /// When the operating system gives no random bytes.
    pub fn generate() -> Result<Self, NoRandomness> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| NoRandomness)?;
        Ok(Self::from_secret(&Secret::new(seed)))
    }

    /// The key pair that belongs to a stored secret key.
    #[must_use]
    pub fn from_secret(secret: &Secret<[u8; 32]>) -> Self {
        Self {
            key: SigningKey::from_bytes(secret.expose()),
        }
    }

    /// The secret key, to hand to a transport that must prove it holds this key.
    #[must_use]
    pub fn secret(&self) -> Secret<[u8; 32]> {
        Secret::new(self.key.to_bytes())
    }

    #[must_use]
    pub fn participant_id(&self) -> ParticipantId {
        participant_id(&self.public_key())
    }
}

impl Signer for Identity {
    fn public_key(&self) -> PublicKey {
        PublicKey(self.key.verifying_key().to_bytes())
    }

    fn sign(&self, message: &[u8]) -> Signature {
        Signature(self.key.sign(message).to_bytes())
    }
}

/// Checks Ed25519 signatures.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ed25519;

impl Verifier for Ed25519 {
    /// Strict verification: signatures that some implementations accept and others do not
    /// are refused, so every node agrees on what verifies.
    fn verify(&self, key: &PublicKey, message: &[u8], signature: &Signature) -> bool {
        let Ok(key) = VerifyingKey::from_bytes(&key.0) else {
            return false;
        };
        let signature = ed25519_dalek::Signature::from_bytes(&signature.0);
        key.verify_strict(message, &signature).is_ok()
    }

    fn participant_id(&self, key: &PublicKey) -> ParticipantId {
        participant_id(key)
    }
}

/// The participant id that belongs to `key`.
#[must_use]
pub fn participant_id(key: &PublicKey) -> ParticipantId {
    let hash = Sha256::digest(key.0);
    let mut id = [0u8; 16];
    for (to, from) in id.iter_mut().zip(hash.iter()) {
        *to = *from;
    }
    ParticipantId::from_key_hash(id)
}

/// The tag that names a meeting on the wire, from its secret.
#[must_use]
pub fn meeting_tag(secret: &Secret<[u8; SECRET_BYTES]>) -> Option<MeetingTag> {
    let mut tag = [0u8; MEETING_TAG_BYTES];
    Hkdf::<Sha256>::new(None, secret.expose())
        .expand(MEETING_TAG_LABEL, &mut tag)
        .ok()?;
    MeetingTag::new(&tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 8032, section 7.1, test 1: the empty message.
    const RFC_SECRET: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
    const RFC_PUBLIC: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const RFC_SIGNATURE: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
                                 5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";

    fn rfc_identity() -> Identity {
        Identity::from_secret(&Secret::new(hex(RFC_SECRET).try_into().unwrap()))
    }

    #[test]
    fn it_signs_as_the_standard_says() {
        let identity = rfc_identity();
        assert_eq!(identity.public_key().0.to_vec(), hex(RFC_PUBLIC));
        assert_eq!(identity.sign(b"").0.to_vec(), hex(RFC_SIGNATURE));
    }

    #[test]
    fn a_signature_verifies_only_for_its_key_and_its_message() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let signature = a.sign(b"report");
        assert!(Ed25519.verify(&a.public_key(), b"report", &signature));
        assert!(!Ed25519.verify(&b.public_key(), b"report", &signature));
        assert!(!Ed25519.verify(&a.public_key(), b"report!", &signature));
        let mut bent = signature;
        bent.0[10] ^= 1;
        assert!(!Ed25519.verify(&a.public_key(), b"report", &bent));
    }

    #[test]
    fn bytes_that_are_not_a_key_verify_nothing() {
        let signature = rfc_identity().sign(b"");
        // Not a point on the curve.
        let mut not_a_key = [0xff; 32];
        not_a_key[31] = 0x7f;
        assert!(!Ed25519.verify(&PublicKey(not_a_key), b"", &signature));
    }

    #[test]
    fn new_keys_differ_and_a_stored_secret_gives_the_same_key_back() {
        let (a, b) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        assert_ne!(a.public_key(), b.public_key());
        let again = Identity::from_secret(&a.secret());
        assert_eq!(again.public_key(), a.public_key());
    }

    #[test]
    fn the_participant_id_is_the_start_of_the_hash_of_the_key() {
        let identity = rfc_identity();
        // SHA-256 of the RFC public key, first 16 bytes; fixed here so that a change to
        // the derivation is noticed.
        let id = identity.participant_id();
        assert_eq!(id.as_bytes().len(), 16);
        assert_eq!(id, Ed25519.participant_id(&identity.public_key()));
        assert_eq!(id.as_bytes(), hex(PINNED_ID));
    }

    #[test]
    fn the_meeting_tag_follows_from_the_secret_and_nothing_else() {
        let tag = meeting_tag(&Secret::new([7; SECRET_BYTES])).unwrap();
        assert_eq!(tag.as_bytes().len(), MEETING_TAG_BYTES);
        assert_eq!(tag, meeting_tag(&Secret::new([7; SECRET_BYTES])).unwrap());
        assert_ne!(tag, meeting_tag(&Secret::new([8; SECRET_BYTES])).unwrap());
        // Fixed here so that a change to the derivation is noticed.
        assert_eq!(tag.as_bytes(), hex(PINNED_TAG));
    }

    #[test]
    fn the_secret_key_is_not_printed() {
        let identity = rfc_identity();
        let shown = format!("{identity:?}");
        assert!(
            !shown.contains("9d61") && !shown.contains("157, 97"),
            "{shown}"
        );
    }

    const PINNED_ID: &str = "21fe31dfa154a261626bf854046fd227";
    const PINNED_TAG: &str = "9e5db3abec4c5a7245774ffedadc12d3";
}
