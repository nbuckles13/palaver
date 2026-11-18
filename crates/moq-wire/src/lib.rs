//! MoQT draft 22 message and object codec for the Palaver profile, including our extensions.
//!
//! Sans-IO and allocation-light: decoding reads from a byte slice through [`Reader`], encoding
//! appends to a `Vec<u8>`. Section numbers in comments refer to draft-ietf-moq-transport-22.

mod error;
mod frame;
mod kvp;
mod name;
mod params;
mod reader;
mod request;
mod setup;
mod varint;

pub use error::{Error, Result};
pub use frame::{Frame, MAX_BODY_BYTES, encode_frame};
pub use kvp::{Kvp, Value, decode_kvps, encode_kvps};
pub use name::{FullTrackName, Location, MAX_NAME_BYTES, MAX_NAMESPACE_FIELDS, Namespace};
pub use params::{GroupOrder, LocationFilter, OK_PARAMS, Params, SUBSCRIBE_PARAMS};
pub use reader::Reader;
pub use request::{
    ERROR_REDIRECT, MAX_REASON_BYTES, Message, REQUEST_ERROR, REQUEST_OK, Redirect, RequestError,
    RequestOk, SUBSCRIBE, SUBSCRIBE_OK, Subscribe, SubscribeOk,
};
pub use setup::{PALAVER_EXTENSIONS, PalaverExtensions, SETUP, Setup, negotiated_palaver_version};
pub use varint::{put_vi64, vi64_len};
