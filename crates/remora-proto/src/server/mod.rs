//! Stateless server-side helpers for verifying requests and building envelopes.

pub mod response;
pub mod verify;

pub use response::{EmptyData, ResponseEnvelope, error_envelope, ok_envelope};
pub use verify::{VerifyError, verify_request};
