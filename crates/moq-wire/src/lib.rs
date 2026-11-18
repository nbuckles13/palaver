//! MoQT draft 22 message and object codec for the Palaver profile, including our extensions.
//!
//! Sans-IO and allocation-light: decoding reads from a byte slice through [`Reader`], encoding
//! appends to a `Vec<u8>`. Section numbers in comments refer to draft-ietf-moq-transport-22.

mod error;
mod frame;
mod kvp;
mod name;
mod reader;
mod setup;
mod varint;

pub use error::{Error, Result};
pub use frame::{Frame, MAX_BODY_BYTES, encode_frame};
pub use kvp::{Kvp, Value, decode_kvps, encode_kvps};
pub use name::{FullTrackName, Location, MAX_NAME_BYTES, MAX_NAMESPACE_FIELDS, Namespace};
pub use reader::Reader;
pub use setup::{PALAVER_EXTENSIONS, PalaverExtensions, SETUP, Setup, negotiated_palaver_version};
pub use varint::{put_vi64, vi64_len};
