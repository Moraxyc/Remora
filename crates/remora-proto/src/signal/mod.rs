//! Engine.IO v4 / Socket.IO v5 framing over WebSocket, default namespace only.

pub mod engineio;
pub mod events;
pub mod socketio;

pub use engineio::{Handshake, MAX_PAYLOAD, PING, PONG};
pub use events::{
    BeControlledEvent, BmsgPushEvent, ControlAck, ControlEvent, ForwardSettingEvent,
    IceFinishedLogEvent, LeftEvent, RefreshReconnectKeyAck, RefreshReconnectKeyEvent,
    ReleasedEvent, RoomInfoEvent, SignalEvent, SoacEvent, SoacPayload,
};
pub use socketio::{
    Ack, CONNECT, DecodeError, Decoder, Event, Packet, encode_ack, encode_binary_event,
    encode_connected, encode_event, placeholder,
};
