//! Cloud REST API: request signing and the response envelope.

pub mod signing;
pub mod types;

pub use signing::{PARAM_PREFIX, SIGN_HEADER, SignKey, sign, verify};
pub use types::{ApiError, parse_response};
