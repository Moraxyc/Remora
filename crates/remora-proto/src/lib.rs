//! UU Remote wire protocol: messages and codecs, no I/O.

pub mod control;
#[allow(clippy::large_enum_variant, reason = "generated oneofs")]
pub mod pb;
pub mod signal;
