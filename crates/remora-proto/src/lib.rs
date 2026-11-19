//! UU Remote wire protocol: messages and codecs, no I/O.

pub mod cloud;
pub mod socketio;

/// Generated from `proto/gv_pb.proto`.
#[allow(clippy::large_enum_variant, reason = "generated oneofs")]
pub mod gv_pb {
    include!(concat!(env!("OUT_DIR"), "/gv_pb.rs"));
}
