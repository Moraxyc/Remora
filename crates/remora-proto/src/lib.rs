//! UU Remote wire protocol: messages, codecs, state machines, and client profiles, sans-IO.

pub mod control;
pub mod error;
pub mod machine;
#[allow(clippy::large_enum_variant, reason = "generated oneofs")]
pub mod pb;
pub mod profile;
pub mod server;
pub mod signal;
